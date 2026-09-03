//! Credentials engine — the highest-risk category.
//!
//! One rule governs the whole module: **secret material never reaches the
//! agent.** Tools here let an agent discover *that* a credential exists and use
//! it indirectly; they never return the value. A model that can read secrets
//! can leak them into a prompt, a log, a tool call, or an HTTP body — and once
//! read, a secret must be treated as compromised.
//!
//! `secure_vault` therefore has no `get` that returns plaintext. Reads confirm
//! existence and return metadata; writes are permitted so an agent can *store*
//! something it was given. Privilege escalation (`privilege_run`) is not
//! implemented at all: an agent that can obtain root defeats every other
//! control in the product, and no use case here justifies it.

use async_trait::async_trait;
use mcp_types::{CallCtx, Category, Envelope, ErrorCode, Tier, ToolDescriptor, ToolModule};
use serde_json::{json, Value};

/// Marker returned instead of any secret value.
pub const REDACTED: &str = "***REDACTED***";

pub struct SecModule {
    /// Keychain service names the agent may touch. Empty = none.
    allowed_services: Vec<String>,
}

impl SecModule {
    pub fn new(allowed_services: Vec<String>) -> Self {
        SecModule { allowed_services }
    }

    fn service_allowed(&self, service: &str) -> bool {
        self.allowed_services.iter().any(|s| s == service)
    }

    /// Does this keychain item exist? Uses `find-generic-password` **without**
    /// `-w`, so the password is never printed and never enters our memory.
    fn vault_exists(&self, args: &Value) -> Envelope {
        let tool = "secure_vault";
        let Some(service) = args.get("service").and_then(Value::as_str) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'service'");
        };
        if !self.service_allowed(service) {
            return Envelope::fail(
                tool,
                ErrorCode::PolicyDenied,
                format!("service '{service}' is not in credentials.allowed_services"),
            );
        }
        let mut cmd = std::process::Command::new("/usr/bin/security");
        cmd.arg("find-generic-password").arg("-s").arg(service);
        if let Some(account) = args.get("account").and_then(Value::as_str) {
            cmd.arg("-a").arg(account);
        }
        match cmd.output() {
            Ok(o) if o.status.success() => {
                // Parse only non-secret attributes from the dump.
                let text = String::from_utf8_lossy(&o.stdout);
                let account = text
                    .lines()
                    .find(|l| l.trim_start().starts_with("\"acct\""))
                    .and_then(|l| l.split('=').nth(1))
                    .map(|v| v.trim().trim_matches('"').to_string());
                Envelope::ok(
                    tool,
                    json!({
                        "service": service,
                        "account": account,
                        "exists": true,
                        "value": REDACTED,
                        "note": "secret values are never returned; use the credential indirectly",
                    }),
                )
            }
            Ok(_) => Envelope::ok(tool, json!({ "service": service, "exists": false })),
            Err(e) => Envelope::fail(tool, ErrorCode::ActionFailed, e.to_string()),
        }
    }

    /// Store a secret. Writing is safe in a way reading is not: the agent
    /// already holds the value it is storing.
    fn vault_set(&self, args: &Value) -> Envelope {
        let tool = "secure_vault";
        let (Some(service), Some(account), Some(secret)) = (
            args.get("service").and_then(Value::as_str),
            args.get("account").and_then(Value::as_str),
            args.get("secret").and_then(Value::as_str),
        ) else {
            return Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                "'set' needs 'service', 'account' and 'secret'",
            );
        };
        if !self.service_allowed(service) {
            return Envelope::fail(
                tool,
                ErrorCode::PolicyDenied,
                format!("service '{service}' is not in credentials.allowed_services"),
            );
        }
        let out = std::process::Command::new("/usr/bin/security")
            .args(["add-generic-password", "-U", "-s"])
            .arg(service)
            .arg("-a")
            .arg(account)
            .arg("-w")
            .arg(secret)
            .output();
        match out {
            Ok(o) if o.status.success() => Envelope::ok(
                tool,
                json!({ "service": service, "account": account, "stored": true }),
            ),
            Ok(o) => Envelope::fail(
                tool,
                ErrorCode::ActionFailed,
                String::from_utf8_lossy(&o.stderr).trim().to_string(),
            ),
            Err(e) => Envelope::fail(tool, ErrorCode::ActionFailed, e.to_string()),
        }
    }

    /// List SSH/GPG identities by **fingerprint only**. Public keys are not
    /// secret, but private key material is never touched.
    fn identities(&self) -> Envelope {
        let tool = "ssh_gpg_identities";
        let mut out = Vec::new();
        if let Some(home) = std::env::var_os("HOME") {
            let dir = std::path::Path::new(&home).join(".ssh");
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() {
                    let p = e.path();
                    // Only ever look at *.pub — never a private key.
                    if p.extension().and_then(|s| s.to_str()) != Some("pub") {
                        continue;
                    }
                    let fp = std::process::Command::new("/usr/bin/ssh-keygen")
                        .arg("-lf")
                        .arg(&p)
                        .output()
                        .ok()
                        .filter(|o| o.status.success())
                        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
                    out.push(json!({
                        "file": p.file_name().map(|n| n.to_string_lossy().to_string()),
                        "fingerprint": fp,
                    }));
                }
            }
        }
        Envelope::ok(
            tool,
            json!({
                "ssh_public_keys": out,
                "note": "public keys and fingerprints only; private key material is never read",
            }),
        )
    }
}

#[async_trait]
impl ToolModule for SecModule {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        vec![
            ToolDescriptor::new(
                "secure_vault",
                Category::Credentials,
                Tier::Dangerous,
                "Check whether a keychain credential exists, or store one. Secret values are \
                 NEVER returned — there is no plaintext read.",
                json!({
                    "type": "object",
                    "properties": {
                        "action": { "type": "string", "enum": ["exists", "set"] },
                        "service": { "type": "string" },
                        "account": { "type": "string" },
                        "secret": { "type": "string", "description": "only for action=set" }
                    },
                    "required": ["action", "service"]
                }),
            ),
            ToolDescriptor::new(
                "ssh_gpg_identities",
                Category::Credentials,
                Tier::Read,
                "List SSH public keys and fingerprints. Private keys are never read.",
                json!({ "type": "object", "properties": {}, "required": [] }),
            ),
        ]
    }

    async fn call(&self, name: &str, args: Value, _ctx: &CallCtx) -> Envelope {
        match name {
            "secure_vault" => match args.get("action").and_then(Value::as_str) {
                Some("exists") => self.vault_exists(&args),
                Some("set") => self.vault_set(&args),
                Some("get") => Envelope::fail_with(
                    "secure_vault",
                    ErrorCode::PolicyDenied,
                    "reading secret values is not supported by design",
                    "use action='exists' to confirm a credential, and consume it indirectly \
                     (e.g. a tool that uses the credential without revealing it)",
                ),
                _ => Envelope::fail(
                    "secure_vault",
                    ErrorCode::InvalidArgs,
                    "'action' must be 'exists' or 'set'",
                ),
            },
            "ssh_gpg_identities" => self.identities(),
            other => Envelope::fail(other, ErrorCode::InvalidArgs, "unknown tool"),
        }
    }

    /// Any keychain touch involves a human. The prompt names the service so the
    /// person can tell a legitimate credential from an unexpected one.
    fn consent_prompt(&self, name: &str, args: &Value) -> Option<String> {
        if name != "secure_vault" {
            return None;
        }
        let service = args.get("service").and_then(Value::as_str).unwrap_or("?");
        match args.get("action").and_then(Value::as_str) {
            Some("set") => Some(format!(
                "Store a credential in the keychain for '{service}'."
            )),
            Some("exists") => Some(format!(
                "Check the keychain for a credential for '{service}'."
            )),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> CallCtx {
        CallCtx::new("t", mcp_types::CancelToken::new())
    }

    /// The defining property of this engine.
    #[tokio::test]
    async fn there_is_no_way_to_read_a_secret_value() {
        let m = SecModule::new(vec!["svc".into()]);
        let env = m
            .call(
                "secure_vault",
                json!({ "action": "get", "service": "svc" }),
                &ctx(),
            )
            .await;
        assert!(!env.ok, "a plaintext read must never succeed");
        let e = env.error.unwrap();
        assert_eq!(e.code, ErrorCode::PolicyDenied);
        assert!(e.suggestion.is_some());
    }

    #[tokio::test]
    async fn existence_checks_return_a_redaction_marker_not_a_value() {
        let m = SecModule::new(vec!["definitely-absent-service".into()]);
        let env = m
            .call(
                "secure_vault",
                json!({ "action": "exists", "service": "definitely-absent-service" }),
                &ctx(),
            )
            .await;
        assert!(env.ok, "{env:?}");
        let d = env.data.unwrap();
        // Absent here, but if present the value must be the marker.
        if d["exists"] == json!(true) {
            assert_eq!(d["value"], json!(REDACTED));
        }
        assert!(d.to_string().find("password").is_none());
    }

    #[tokio::test]
    async fn services_outside_the_allowlist_are_denied() {
        let m = SecModule::new(vec!["allowed".into()]);
        for action in ["exists", "set"] {
            let env = m
                .call(
                    "secure_vault",
                    json!({ "action": action, "service": "other", "account": "a", "secret": "s" }),
                    &ctx(),
                )
                .await;
            assert!(
                !env.ok,
                "{action} on a non-allowlisted service must be denied"
            );
            assert_eq!(env.error.unwrap().code, ErrorCode::PolicyDenied);
        }
    }

    #[tokio::test]
    async fn no_services_allowed_means_nothing_is_reachable() {
        let m = SecModule::new(vec![]);
        let env = m
            .call(
                "secure_vault",
                json!({ "action": "exists", "service": "any" }),
                &ctx(),
            )
            .await;
        assert!(!env.ok);
    }

    #[test]
    fn every_keychain_operation_requires_consent() {
        let m = SecModule::new(vec!["svc".into()]);
        let p = m
            .consent_prompt(
                "secure_vault",
                &json!({ "action": "exists", "service": "svc" }),
            )
            .unwrap();
        assert!(p.contains("svc"), "{p}");
        let p = m
            .consent_prompt(
                "secure_vault",
                &json!({ "action": "set", "service": "svc" }),
            )
            .unwrap();
        assert!(p.contains("svc"), "{p}");
        // Listing public keys is harmless and must not nag.
        assert!(m.consent_prompt("ssh_gpg_identities", &json!({})).is_none());
    }

    /// Only public keys are ever inspected.
    #[tokio::test]
    async fn identities_never_expose_private_key_material() {
        let env = SecModule::new(vec![])
            .call("ssh_gpg_identities", json!({}), &ctx())
            .await;
        assert!(env.ok, "{env:?}");
        let text = env.data.unwrap().to_string();
        assert!(!text.contains("PRIVATE KEY"), "private material leaked");
        for key in env_files(&text) {
            assert!(key.ends_with(".pub"), "non-public key listed: {key}");
        }
    }

    fn env_files(json_text: &str) -> Vec<String> {
        // crude extraction of "file":"..." values for the assertion above
        let mut out = Vec::new();
        let mut rest = json_text;
        while let Some(i) = rest.find("\"file\":\"") {
            rest = &rest[i + 8..];
            if let Some(j) = rest.find('"') {
                out.push(rest[..j].to_string());
                rest = &rest[j..];
            } else {
                break;
            }
        }
        out
    }

    /// `privilege_run` is deliberately absent — root defeats every other control.
    #[tokio::test]
    async fn privilege_escalation_is_not_offered() {
        let m = SecModule::new(vec![]);
        assert!(
            !m.descriptors().iter().any(|d| d.name.contains("privilege")),
            "privilege_run must not be exposed"
        );
        let env = m.call("privilege_run", json!({}), &ctx()).await;
        assert!(!env.ok);
    }
}
