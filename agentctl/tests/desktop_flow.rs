//! `desktop_flow` and `desktop_record` through the real server: a replayed step
//! is an ordinary tool call at the gate, and recording is a dangerous-tier tool
//! an operator must name.
//!
//! Every call here is one that is refused or fails before it touches the
//! desktop (a role that denies the step's tool, a foreground mode that refuses
//! it, a missing secret, a tool that was not enabled), so the tests are safe to
//! run on a machine somebody is using. What a replay does to a real
//! application, and what a real listener records, is in
//! `live_desktop_input.rs`.

#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::sync::Arc;

use mcp_policy::{AuditSink, NoConsent, PolicyConfig, RoleProfile};
use mcp_types::{Category, ErrorCode, Foreground};
use serde_json::{json, Value};
use test_support::{test_policy, InProcClient};

fn rig(
    mut cfg: PolicyConfig,
    name: &str,
) -> (InProcClient, Arc<mcp_policy::Policy>, std::path::PathBuf) {
    cfg.allowed_apps = vec!["TextEdit".to_string()];
    // Each server keeps its flows in a state directory of its own, as two
    // servers on one machine would, so the tests do not share a file.
    let dir = std::env::temp_dir().join(format!("agentctl-flow-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    cfg.kill_switch_file = dir.join("STOP");
    cfg.audit_dir = dir.join("audit");
    let (server, policy, _) =
        agentctl::build_server_with(cfg, AuditSink::memory(), Arc::new(NoConsent), name)
            .expect("server builds");
    (InProcClient::new(server), policy, dir)
}

fn input_cfg() -> PolicyConfig {
    test_policy(&[Category::Input])
}

fn code(env: &mcp_types::Envelope) -> ErrorCode {
    env.error.as_ref().expect("an error").code
}

/// A flow whose only step is a chord: it needs no element, so it can be
/// replayed with no application open, and a refusal stops it before the OS.
async fn save_chord_flow(c: &InProcClient, name: &str) {
    c.ok(
        "desktop_flow",
        json!({ "action": "save", "name": name, "steps": [
            { "op": "shortcut", "combo": "cmd+z" }
        ]}),
    )
    .await;
}

#[tokio::test]
async fn a_replayed_step_meets_the_role_that_denies_its_tool_and_is_audited_as_its_own_call() {
    let mut role = RoleProfile::new("no-chords");
    role.denied_tools = vec!["keyboard_shortcut".into()];
    let mut cfg = input_cfg();
    cfg.active_role = Some(role.name.clone());
    cfg.roles.insert(role.name.clone(), role);
    let (c, policy, _) = rig(cfg, "flow-role");
    save_chord_flow(&c, "undo").await;

    let env = c
        .call("desktop_flow", json!({ "action": "run", "name": "undo" }))
        .await;
    assert_eq!(code(&env), ErrorCode::PolicyDenied, "{env:?}");
    let data = env.data.expect("the report of the run");
    assert_eq!(data["failed_at"], json!(0));
    assert_eq!(data["steps"][0]["ok"], json!(false));
    // The refusal is the gate's, under the step's own tool name, not the flow's.
    let denied = policy
        .audit_sink()
        .memory_records()
        .into_iter()
        .any(|r| r["tool"] == json!("keyboard_shortcut") && r["decision"] == json!("deny"));
    assert!(
        denied,
        "the denied step is not in the audit log under its own name: {:#?}",
        policy.audit_sink().memory_records()
    );
}

#[tokio::test]
async fn a_role_that_denies_desktop_flow_refuses_the_run_itself() {
    let mut role = RoleProfile::new("no-flows");
    role.denied_tools = vec!["desktop_flow".into()];
    let mut cfg = input_cfg();
    cfg.active_role = Some(role.name.clone());
    cfg.roles.insert(role.name.clone(), role);
    let (c, _, _) = rig(cfg, "flow-role-run");
    let env = c.call("desktop_flow", json!({ "action": "list" })).await;
    assert_eq!(code(&env), ErrorCode::PolicyDenied);
}

#[tokio::test]
async fn under_never_a_flow_that_would_use_the_keyboard_is_refused_before_any_step_runs() {
    let mut cfg = input_cfg();
    cfg.foreground = Foreground::Never;
    let (c, _, _) = rig(cfg, "flow-never");
    c.ok(
        "desktop_flow",
        json!({ "action": "save", "name": "two", "steps": [
            { "op": "shortcut", "combo": "escape" },
            { "op": "shortcut", "combo": "cmd+z" }
        ]}),
    )
    .await;
    let env = c
        .call("desktop_flow", json!({ "action": "run", "name": "two" }))
        .await;
    let err = env.error.expect("refused");
    assert_eq!(err.code, ErrorCode::PolicyDenied);
    assert!(
        err.message.contains("step 0") && err.message.contains("foreground = \"never\""),
        "{}",
        err.message
    );
    assert!(env.data.is_none(), "no step may have run");
}

#[tokio::test]
async fn a_flow_that_needs_secrets_names_them_and_runs_nothing_without_them() {
    let (c, _, _) = rig(input_cfg(), "flow-secrets");
    c.ok(
        "desktop_flow",
        json!({ "action": "save", "name": "login", "steps": [
            { "op": "shortcut", "combo": "cmd+z" },
            { "op": "type", "secret_ref": "password",
              "locator": { "app": "TextEdit", "role": "textfield", "title": "Password" } }
        ]}),
    )
    .await;
    let env = c
        .call("desktop_flow", json!({ "action": "run", "name": "login" }))
        .await;
    let err = env.error.expect("refused");
    assert_eq!(err.code, ErrorCode::InvalidArgs);
    assert!(err.message.contains("password"), "{}", err.message);
    assert!(
        err.suggestion.unwrap_or_default().contains("secrets"),
        "the way forward is named"
    );
    assert!(env.data.is_none(), "no step may have run");
    // The list shows which secrets a flow wants, never a value.
    let list = c.ok("desktop_flow", json!({ "action": "list" })).await;
    assert_eq!(list["flows"][0]["secrets"], json!(["password"]));
}

#[tokio::test]
async fn a_flow_with_a_secret_and_a_literal_value_cannot_be_saved() {
    let (c, _, _) = rig(input_cfg(), "flow-leak");
    let env = c
        .call(
            "desktop_flow",
            json!({ "action": "save", "name": "bad", "steps": [
                { "op": "type", "secret_ref": "pw", "text": "hunter2" }
            ]}),
        )
        .await;
    let err = env.error.expect("refused");
    assert_eq!(err.code, ErrorCode::InvalidArgs);
    assert!(!err.message.contains("hunter2"), "{}", err.message);
    let list = c.ok("desktop_flow", json!({ "action": "list" })).await;
    assert_eq!(list["count"], json!(0));
}

#[tokio::test]
async fn recording_is_a_dangerous_tool_and_is_refused_unless_named_in_enable() {
    let (c, _, _) = rig(input_cfg(), "record-default");
    let env = c
        .call("desktop_record", json!({ "action": "status" }))
        .await;
    assert_eq!(code(&env), ErrorCode::PolicyDenied, "{env:?}");
}

#[tokio::test]
async fn a_recording_with_no_allowed_apps_would_keep_nothing_so_it_does_not_start() {
    let mut cfg = input_cfg();
    cfg.enable = vec!["desktop_record".into()];
    let (server, _, _) = agentctl::build_server_with(
        PolicyConfig {
            allowed_apps: Vec::new(),
            ..cfg
        },
        AuditSink::memory(),
        Arc::new(NoConsent),
        "record-none",
    )
    .expect("server builds");
    let c = InProcClient::new(server);
    let env = c.call("desktop_record", json!({ "action": "start" })).await;
    let err = env.error.expect("refused");
    // Either the platform has no listener, or the empty list stops it first;
    // never a started recording.
    assert!(
        matches!(err.code, ErrorCode::PolicyDenied | ErrorCode::UnsupportedOs),
        "{err:?}"
    );
    let status: Value = c.ok("desktop_record", json!({ "action": "status" })).await;
    assert_eq!(status["recording"], json!(false));
}
