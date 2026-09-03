use mcp_types::{Category, Envelope, ErrorCode, Tier, ToolDescriptor};
use serde_json::Value;

use crate::{
    AuditRecord, AuditSink, Decision, DenialBudget, KillSwitch, Mode, PolicyConfig, Redactor,
};

/// The policy kernel: ties config, denial budget, kill switch, audit sink, and
/// redactor into the single object `mcp-core::dispatch` consults.
pub struct Policy {
    config: PolicyConfig,
    budget: DenialBudget,
    kill: KillSwitch,
    audit: AuditSink,
    redactor: Redactor,
    consent: std::sync::Arc<dyn crate::ConsentProvider>,
    prompts: crate::PromptBudget,
}

impl Policy {
    pub fn new(config: PolicyConfig, audit: AuditSink, redactor: Redactor) -> Self {
        let budget = DenialBudget::new(config.max_denials);
        let config_max_prompts = config.max_consent_prompts;
        let kill = KillSwitch::new(config.kill_switch_file.clone());
        let prompts = crate::PromptBudget::new(config_max_prompts);
        Policy {
            config,
            budget,
            kill,
            audit,
            redactor,
            consent: std::sync::Arc::new(crate::NoConsent),
            prompts,
        }
    }

    /// Attach the out-of-band human-approval channel.
    pub fn with_consent(mut self, provider: std::sync::Arc<dyn crate::ConsentProvider>) -> Self {
        self.consent = provider;
        self
    }

    /// Ask a human to approve one specific action.
    ///
    /// Fail-closed at every step: autonomous mode never asks, an exhausted
    /// prompt budget denies without asking (consent fatigue is an attack), and
    /// anything short of an explicit approval denies. The outcome is audited.
    pub fn request_consent(
        &self,
        session_id: &str,
        tool: &str,
        summary: &str,
        details: Option<String>,
    ) -> crate::ConsentOutcome {
        use crate::{ConsentOutcome, ConsentRequest};
        if !self.config.mode.prompts_human() {
            return ConsentOutcome::Unavailable;
        }
        if !self.prompts.take() {
            tracing::warn!(tool, "consent prompt budget exhausted; denying");
            return ConsentOutcome::Denied;
        }
        let outcome = self.consent.request(&ConsentRequest {
            tool: tool.to_string(),
            summary: summary.to_string(),
            details,
            session_id: session_id.to_string(),
        });
        tracing::info!(tool, channel = self.consent.kind(), ?outcome, "consent");
        outcome
    }

    pub fn config(&self) -> &PolicyConfig {
        &self.config
    }

    pub fn is_category_enabled(&self, category: Category) -> bool {
        self.config.categories.contains(&category)
    }

    /// Stop everything, and say why.
    ///
    /// Used by the pointer watcher when a human takes over the mouse. Audited
    /// like any other decision so the log shows what ended the session.
    pub fn trip_kill_switch(&self, session_id: &str, reason: &str) {
        self.kill.trip(reason);
        let mut rec = crate::AuditRecord::pre(session_id, "kill_switch");
        rec.decision = Some(format!("tripped: {reason}"));
        self.audit(&rec);
        tracing::error!(reason, "kill switch tripped");
    }

    /// Why the kill switch is engaged, if it is.
    pub fn kill_switch_reason(&self) -> Option<String> {
        self.kill.reason()
    }

    pub fn kill_switch_tripped(&self) -> bool {
        self.kill.tripped()
    }

    /// The coarse-then-fine gate: category allowlist first, then tier. A
    /// `dangerous` tool must additionally be named in `enable`. `NeedConsent` is
    /// not produced here — it comes from per-action checks in later phases.
    pub fn gate(&self, desc: &ToolDescriptor) -> Decision {
        if !self.is_category_enabled(desc.category) {
            return Decision::Deny {
                code: ErrorCode::PolicyDenied,
                reason: format!("category '{}' is not enabled", desc.category.slug()),
            };
        }
        match desc.tier {
            Tier::Read | Tier::Standard => Decision::Allow,
            Tier::Dangerous => {
                if self.config.enable.iter().any(|n| n == &desc.name) {
                    Decision::Allow
                } else {
                    Decision::Deny {
                        code: ErrorCode::PolicyDenied,
                        reason: format!(
                            "dangerous tool '{}' is not enabled (add it to policy.enable)",
                            desc.name
                        ),
                    }
                }
            }
        }
    }

    /// In autonomous mode there is no consent channel, so `NeedConsent` collapses
    /// to a denial. Interactive-mode consent handling lands with the input engine.
    pub fn resolve_consent(&self, decision: Decision) -> Decision {
        match (self.config.mode, decision) {
            (Mode::Autonomous, Decision::NeedConsent { prompt }) => Decision::Deny {
                code: ErrorCode::ConsentRequired,
                reason: format!("consent required but running autonomously: {prompt}"),
            },
            // Only a read tool can reach here in dry run — anything else was
            // already short-circuited — and a read that wants consent is not
            // something a rehearsal should approve on the operator's behalf.
            (Mode::DryRun, Decision::NeedConsent { prompt }) => Decision::Deny {
                code: ErrorCode::ConsentRequired,
                reason: format!("consent required, and dry run never prompts: {prompt}"),
            },
            (_, other) => other,
        }
    }

    /// The configured mode.
    pub fn mode(&self) -> Mode {
        self.config.mode
    }

    /// Whether mutations should be reported rather than performed.
    pub fn is_dry_run(&self) -> bool {
        matches!(self.config.mode, Mode::DryRun)
    }

    /// Record a denial against the anti-spin budget; returns `true` if exhausted.
    pub fn record_denial(&self) -> bool {
        self.budget.record()
    }

    pub fn denial_count(&self) -> usize {
        self.budget.count()
    }

    /// Mark a result as carrying content from outside the trust boundary.
    ///
    /// Applied centrally, after redaction, to any tool whose descriptor says
    /// its output contains third-party text. Doing it here rather than in each
    /// engine means the marker cannot be forgotten when a tool is added, and
    /// cannot be spoofed by the content itself.
    pub fn mark_untrusted(&self, mut env: Envelope) -> Envelope {
        if !env.ok {
            return env;
        }
        if let Some(data) = env.data.as_mut() {
            crate::injection::flag_untrusted(data);
        }
        env
    }

    /// Redact secrets in a JSON value in place (results and audit payloads).
    pub fn redact(&self, value: &mut Value) {
        self.redactor.redact_value(value);
    }

    /// Redact an envelope's `data` in place.
    pub fn redact_envelope(&self, mut env: Envelope) -> Envelope {
        if let Some(data) = env.data.as_mut() {
            self.redactor.redact_value(data);
        }
        env
    }

    pub fn audit(&self, record: &AuditRecord) {
        self.audit.write(record);
    }

    pub fn audit_sink(&self) -> &AuditSink {
        &self.audit
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn desc(name: &str, category: Category, tier: Tier) -> ToolDescriptor {
        ToolDescriptor::new(name, category, tier, "", json!({"type": "object"}))
    }

    fn policy_with(config: PolicyConfig) -> Policy {
        Policy::new(config, AuditSink::memory(), Redactor::empty())
    }

    #[test]
    fn allows_read_tool_in_enabled_category() {
        let p = policy_with(PolicyConfig::default());
        let d = p.gate(&desc("get_ui_tree", Category::Vision, Tier::Read));
        assert_eq!(d, Decision::Allow);
    }

    #[test]
    fn denies_disabled_category() {
        let p = policy_with(PolicyConfig::default()); // terminal not in defaults
        let d = p.gate(&desc("exec", Category::Terminal, Tier::Standard));
        match d {
            Decision::Deny { code, .. } => assert_eq!(code, ErrorCode::PolicyDenied),
            other => panic!("expected deny, got {other:?}"),
        }
    }

    #[test]
    fn denies_dangerous_unless_enabled() {
        let mut cfg = PolicyConfig::default();
        cfg.categories.push(Category::Desktop);
        let p = policy_with(cfg);
        let dang = desc("power_control", Category::Desktop, Tier::Dangerous);
        assert!(matches!(p.gate(&dang), Decision::Deny { .. }));

        let mut cfg2 = PolicyConfig::default();
        cfg2.categories.push(Category::Desktop);
        cfg2.enable.push("power_control".into());
        let p2 = policy_with(cfg2);
        assert_eq!(p2.gate(&dang), Decision::Allow);
    }

    #[test]
    fn autonomous_mode_collapses_consent_to_deny() {
        let cfg = PolicyConfig {
            mode: Mode::Autonomous,
            ..PolicyConfig::default()
        };
        let p = policy_with(cfg);
        let d = p.resolve_consent(Decision::NeedConsent {
            prompt: "ok?".into(),
        });
        assert!(matches!(
            d,
            Decision::Deny {
                code: ErrorCode::ConsentRequired,
                ..
            }
        ));
    }
}
