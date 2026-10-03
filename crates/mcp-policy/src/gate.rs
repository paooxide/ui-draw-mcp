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
    judge: Option<std::sync::Arc<mcp_judge::Judge>>,
    anonymizer: std::sync::Mutex<crate::SessionAnonymizer>,
}

/// The stricter of two denial budgets, where 0 means no limit.
fn tighter_budget(a: usize, b: usize) -> usize {
    match (a, b) {
        (0, x) | (x, 0) => x,
        (x, y) => x.min(y),
    }
}

impl Policy {
    pub fn new(config: PolicyConfig, audit: AuditSink, redactor: Redactor) -> Self {
        // A role may tighten the operator's denial budget, never loosen or
        // disable it (0 means no limit).
        let effective_max_denials = match config.active_role_profile().and_then(|r| r.max_denials) {
            Some(role) => tighter_budget(config.max_denials, role),
            None => config.max_denials,
        };
        let budget = DenialBudget::new(effective_max_denials);
        let config_max_prompts = config.max_consent_prompts;
        let kill = KillSwitch::new(config.kill_switch_file.clone());
        let prompts = crate::PromptBudget::new(config_max_prompts);
        let anonymize_enabled = config.anonymize;
        Policy {
            config,
            budget,
            kill,
            audit,
            redactor,
            consent: std::sync::Arc::new(crate::NoConsent),
            prompts,
            judge: None,
            anonymizer: std::sync::Mutex::new(
                crate::SessionAnonymizer::new().with_enabled(anonymize_enabled),
            ),
        }
    }

    /// Attach the judge. It is consulted only where a judgment can tighten:
    /// see `judged.rs`.
    pub fn with_judge(mut self, judge: std::sync::Arc<mcp_judge::Judge>) -> Self {
        self.judge = Some(judge);
        self
    }

    pub fn judge(&self) -> Option<&std::sync::Arc<mcp_judge::Judge>> {
        self.judge.as_ref()
    }

    /// The judge's second opinion on an untrusted result: may add the
    /// injection flag, never remove it. Runs after [`Self::mark_untrusted`].
    pub async fn second_opinion(&self, mut env: Envelope) -> Envelope {
        if let Some(data) = env.data.as_mut() {
            crate::judged::second_opinion_on_content(data, self.judge.as_ref()).await;
        }
        env
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

    /// Check whether a tool descriptor is permitted under the active policy and role.
    pub fn allows_tool(&self, desc: &ToolDescriptor) -> bool {
        if !self.is_category_enabled(desc.category) {
            return false;
        }
        if let Some(role) = self.config.active_role_profile() {
            if !role.allows_tool(desc) {
                return false;
            }
        }
        true
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

        // Active role / RBAC profile check
        if let Some(role) = self.config.active_role_profile() {
            if let Err(reason) = role.check_permission(desc) {
                return Decision::Deny {
                    code: ErrorCode::PolicyDenied,
                    reason,
                };
            }
            if role.requires_consent(desc) {
                return Decision::NeedConsent {
                    prompt: format!(
                        "run tool '{}' (required by role '{}')",
                        desc.name, role.name
                    ),
                };
            }
        }

        match desc.tier {
            Tier::Read | Tier::Standard => Decision::Allow,
            Tier::Dangerous => match self.config.access {
                // A profile enables every dangerous tool. "ask" still confirms
                // each one with the human; "auto" and "bypass" let it run.
                Some(crate::Access::Ask) => Decision::NeedConsent {
                    prompt: format!("run the dangerous tool '{}'", desc.name),
                },
                Some(crate::Access::Auto) | Some(crate::Access::Bypass) => Decision::Allow,
                // No profile: the granular opt-in list decides.
                None => {
                    if self.config.enable.iter().any(|n| n == &desc.name) {
                        Decision::Allow
                    } else {
                        Decision::Deny {
                            code: ErrorCode::PolicyDenied,
                            reason: format!(
                                "dangerous tool '{}' is not enabled (add it to policy.enable, \
                                 or set policy.access = \"ask\")",
                                desc.name
                            ),
                        }
                    }
                }
            },
        }
    }

    /// Check hard invariants (e.g. protected system paths, denied domains) against tool arguments.
    pub fn check_invariants(&self, _tool: &str, args: &Value) -> Result<(), String> {
        crate::role::check_arguments_invariants(args, &self.config.invariants)
    }

    /// The active role profile name, if one is configured for this session.
    pub fn active_role(&self) -> Option<&str> {
        self.config.active_role.as_deref()
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
    /// The permission profile in force, if any.
    pub fn access(&self) -> Option<crate::Access> {
        self.config.access
    }

    /// Bypass turns off consent and the destructive gate; only the kill switch
    /// and human-override remain. Checked in the dispatch and the engines.
    pub fn is_bypass(&self) -> bool {
        self.config.access == Some(crate::Access::Bypass)
    }

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

    /// Register an entity (e.g. patient name, user name) for session tokenization.
    pub fn register_entity(&self, entity: &str, entity_type: crate::EntityType) {
        if let Ok(mut anon) = self.anonymizer.lock() {
            anon.register(entity, entity_type);
        }
    }

    /// Anonymize all sensitive PII/PHI in a JSON value in place.
    pub fn anonymize(&self, value: &mut Value) {
        if let Ok(mut anon) = self.anonymizer.lock() {
            anon.anonymize_value(value);
        }
    }

    /// Anonymize an outgoing envelope's data.
    pub fn anonymize_envelope(&self, mut env: Envelope) -> Envelope {
        if let Some(data) = env.data.as_mut() {
            if let Ok(mut anon) = self.anonymizer.lock() {
                anon.anonymize_value(data);
            }
        }
        env
    }

    /// Refuse a call that would carry an issued token to a tool outside
    /// [`crate::TOKEN_SINK_TOOLS`]. Runs before the gate, so the refusal is the
    /// same whatever the access profile.
    pub fn check_token_sink(&self, tool: &str, args: &Value) -> Result<(), String> {
        if crate::is_token_sink(tool) {
            return Ok(());
        }
        let tokens = match self.anonymizer.lock() {
            Ok(anon) => anon.known_tokens_in(args),
            Err(e) => e.into_inner().known_tokens_in(args),
        };
        if tokens.is_empty() {
            return Ok(());
        }
        Err(format!(
            "{} stands for redacted personal data and cannot be passed to '{tool}'; \
             tokens are resolved only when typed into a local field ({})",
            tokens.join(", "),
            crate::TOKEN_SINK_TOOLS.join(", ")
        ))
    }

    /// Restore synthetic tokens to plaintext in the arguments of a sink tool.
    /// Every other tool gets its arguments unchanged: a token reaching one has
    /// already been refused by [`Self::check_token_sink`], and this keeps the
    /// two in agreement if a caller forgets to check.
    pub fn de_anonymize_args(&self, tool: &str, mut args: Value) -> Value {
        if !crate::is_token_sink(tool) {
            return args;
        }
        if let Ok(anon) = self.anonymizer.lock() {
            anon.de_anonymize_value(&mut args);
        }
        args
    }

    /// Check whether PII/PHI anonymization is enabled for this session.
    pub fn is_anonymize_enabled(&self) -> bool {
        self.anonymizer
            .lock()
            .map(|a| a.is_enabled())
            .unwrap_or(false)
    }

    /// Dynamically enable or disable PII/PHI anonymization for this session.
    pub fn set_anonymize_enabled(&self, enabled: bool) {
        if let Ok(mut anon) = self.anonymizer.lock() {
            anon.set_enabled(enabled);
        }
    }

    /// Access the underlying session anonymizer mutex.
    pub fn anonymizer(&self) -> &std::sync::Mutex<crate::SessionAnonymizer> {
        &self.anonymizer
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

    /// The three access profiles, over a dangerous tool the granular config
    /// would have refused: ask confirms it, auto and bypass run it, and the
    /// category is on for all three (a profile enables everything).
    #[test]
    fn access_profiles_gate_a_dangerous_tool_three_ways() {
        let dang = desc("power_control", Category::Desktop, Tier::Dangerous);
        let with = |access| {
            let mut cfg = PolicyConfig {
                access: Some(access),
                ..PolicyConfig::default()
            };
            // A profile enables every category; the loader does this, so mirror
            // it here where we construct the config by hand.
            cfg.categories = crate::all_categories();
            policy_with(cfg)
        };
        match with(crate::Access::Ask).gate(&dang) {
            Decision::NeedConsent { prompt } => assert!(prompt.contains("power_control")),
            other => panic!("ask should confirm, got {other:?}"),
        }
        assert_eq!(with(crate::Access::Auto).gate(&dang), Decision::Allow);
        assert_eq!(with(crate::Access::Bypass).gate(&dang), Decision::Allow);
        // And a standard tool in a category the defaults never enabled is now
        // allowed under a profile, because everything is on.
        let std_tool = desc("exec", Category::Terminal, Tier::Standard);
        assert_eq!(with(crate::Access::Ask).gate(&std_tool), Decision::Allow);
    }

    #[test]
    fn only_bypass_reports_bypass() {
        let mk = |a: Option<crate::Access>| {
            policy_with(PolicyConfig {
                access: a,
                ..PolicyConfig::default()
            })
        };
        assert!(mk(Some(crate::Access::Bypass)).is_bypass());
        assert!(!mk(Some(crate::Access::Ask)).is_bypass());
        assert!(!mk(Some(crate::Access::Auto)).is_bypass());
        assert!(!mk(None).is_bypass());
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
