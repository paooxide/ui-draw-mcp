//! `foreground = "never"` through the real wiring: config -> composition root
//! -> policy gate -> engine, and out into the audit log.
//!
//! The per-tool decisions are tested beside the code (`mcp-input`,
//! `mcp-window`) against recording backends. What only the whole stack can
//! show is that the configured value reaches the engines and that a refusal is
//! audited like any other. The calls here are chosen so that even if the
//! setting were not wired through, nothing would happen to the desktop: a ref
//! that does not exist and an app that is not running fail without acting.

#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::sync::Arc;

use mcp_policy::{AuditSink, NoConsent, PolicyConfig};
use mcp_types::{Category, ErrorCode, Foreground};
use serde_json::json;
use test_support::{test_policy, InProcClient};

fn client(foreground: Foreground, name: &str) -> (InProcClient, Arc<mcp_policy::Policy>) {
    let mut cfg: PolicyConfig = test_policy(&[Category::Input, Category::Window]);
    cfg.allowed_apps = vec!["TextEdit".to_string()];
    cfg.foreground = foreground;
    let (server, policy, _) =
        agentctl::build_server_with(cfg, AuditSink::memory(), Arc::new(NoConsent), name)
            .expect("server builds");
    (InProcClient::new(server), policy)
}

#[tokio::test]
async fn the_configured_value_reaches_both_engines_and_the_refusal_is_audited() {
    let (c, policy) = client(Foreground::Never, "fg-never");

    let env = c
        .call("ui_action", json!({"ref":"@e999","action":"show_menu"}))
        .await;
    let err = env.error.expect("refused");
    assert_eq!(err.code, ErrorCode::PolicyDenied);
    assert!(
        err.message.contains("foreground = \"never\""),
        "{}",
        err.message
    );

    let env = c.call("focus_app", json!({"app":"No Such App 4471"})).await;
    assert_eq!(env.error.expect("refused").code, ErrorCode::PolicyDenied);

    // Audited the way every other engine refusal is: a call record and a
    // result record with the error code. The sink holds no message text.
    let records = policy.audit_sink().memory_records();
    let denied: Vec<_> = records
        .iter()
        .filter(|r| r["ok"] == json!(false) && r["error_code"] == json!("POLICY_DENIED"))
        .map(|r| r["tool"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        denied.contains(&"ui_action".to_string()) && denied.contains(&"focus_app".to_string()),
        "refusals missing from the audit trail: {records:#?}"
    );
}

#[tokio::test]
async fn the_default_is_allow_and_the_same_call_is_not_a_foreground_refusal() {
    assert_eq!(PolicyConfig::default().foreground, Foreground::Allow);
    let (c, _) = client(Foreground::Allow, "fg-allow");
    let env = c
        .call("ui_action", json!({"ref":"@e999","action":"focus"}))
        .await;
    let code = env.error.expect("no such element").code;
    assert_ne!(code, ErrorCode::PolicyDenied);
}

/// The quiet pin is how a background app is driven under `never`: it must not
/// be a foreground refusal, the raising form must name it, and the allowlist
/// holds either way. A pin reads and changes nothing on screen, so this is
/// safe to run on a live desktop whether or not TextEdit is open.
#[tokio::test]
async fn under_never_the_quiet_pin_is_allowed_and_the_raising_one_names_it() {
    let (c, _) = client(Foreground::Never, "fg-never-pin");

    let env = c.call("focus_app", json!({"app":"TextEdit"})).await;
    let err = env.error.expect("the raising form is refused");
    assert_eq!(err.code, ErrorCode::PolicyDenied);
    assert!(
        err.suggestion
            .as_deref()
            .unwrap_or_default()
            .contains("activate: false"),
        "{err:?}"
    );

    let env = c
        .call("focus_app", json!({"app":"TextEdit","activate":false}))
        .await;
    if let Some(err) = env.error {
        // Not running, or no Accessibility permission: an answer about the
        // app, not a foreground refusal.
        assert!(
            !err.message.contains("foreground = \"never\""),
            "the quiet pin was refused as a foreground change: {err:?}"
        );
    }

    let env = c
        .call("focus_app", json!({"app":"Safari","activate":false}))
        .await;
    let err = env.error.expect("outside allowed_apps");
    assert_eq!(err.code, ErrorCode::PolicyDenied);
    assert!(err.message.contains("allowed_apps"), "{err:?}");
}
