//! The browser engine under `foreground = "never"` and `"polite"` through the
//! real wiring: config -> composition root -> policy gate -> browser engine.
//!
//! What the engine sends to Chrome, command by command, is read off the wire
//! in `mcp-browser`'s own tests; what only the whole stack can show is that the
//! configured value reaches the browser engine and that a refusal comes back
//! as `POLICY_DENIED`, audited like any other. The refusals here happen before
//! a browser is started, so they are safe on a screen nobody can see; the
//! headless run is skipped under `AGENTCTL_SKIP_LIVE=1` or without Chrome.

#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::sync::Arc;

use mcp_policy::{AuditSink, NoConsent, PolicyConfig};
use mcp_types::{Category, ErrorCode, Foreground};
use serde_json::json;
use test_support::{skip_live, test_policy, InProcClient};

fn client(foreground: Foreground, name: &str) -> (InProcClient, Arc<mcp_policy::Policy>) {
    let mut cfg: PolicyConfig = test_policy(&[Category::Browser]);
    cfg.foreground = foreground;
    // `browser_eval` is dangerous tier: named, as an operator would.
    cfg.enable = vec!["browser_eval".to_string()];
    let (server, policy, _) =
        agentctl::build_server_with(cfg, AuditSink::memory(), Arc::new(NoConsent), name)
            .expect("server builds");
    (InProcClient::new(server), policy)
}

#[tokio::test]
async fn a_visible_browser_launch_is_refused_and_audited_under_never_and_polite() {
    for fg in [Foreground::Never, Foreground::Polite] {
        let (c, policy) = client(fg, "browser-fg-refusal");
        for launch in [json!({"headless": false}), json!({"browser": "safari"})] {
            let env = c.call("browser_connect", json!({ "launch": launch })).await;
            let err = env.error.expect("refused");
            assert_eq!(err.code, ErrorCode::PolicyDenied, "{fg:?}: {err:?}");
            assert!(
                err.message
                    .contains(&format!("foreground = \"{}\"", fg.as_str())),
                "{}",
                err.message
            );
            assert!(err.message.contains("headless: true"), "{}", err.message);
        }
        let records = policy.audit_sink().memory_records();
        assert!(
            records.iter().any(|r| r["tool"] == json!("browser_connect")
                && r["error_code"] == json!("POLICY_DENIED")),
            "{fg:?}: refusal missing from the audit trail: {records:#?}"
        );
    }
}

/// Under `allow` the same call is not a foreground refusal: with nothing to
/// attach to and no launch it fails on its own terms, which proves the
/// refusal above came from the setting.
#[tokio::test]
async fn under_allow_the_same_call_is_not_refused_for_the_foreground() {
    let (c, _) = client(Foreground::Allow, "browser-fg-allow");
    let env = c.call("browser_connect", json!({})).await;
    let err = env.error.expect("needs attach or launch");
    assert_ne!(err.code, ErrorCode::PolicyDenied, "{err:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_headless_browser_runs_a_normal_session_under_never() {
    if skip_live()
        || !mcp_browser::CHROME_BINS
            .iter()
            .any(|p| std::path::Path::new(p).exists())
    {
        eprintln!("skipping: live browser tests are off or there is no Chrome");
        return;
    }
    let (c, _) = client(Foreground::Never, "browser-fg-headless");
    let connected = c
        .ok(
            "browser_connect",
            json!({"launch": {"headless": true, "port": 0}}),
        )
        .await;
    assert_eq!(connected["foregrounded"], json!(false), "{connected}");
    let id = connected["browser_id"].as_u64().expect("browser_id");
    let opened = c
        .ok(
            "browser_tabs",
            json!({"browser_id": id, "action": "open", "url": "about:blank"}),
        )
        .await;
    let tab = opened["target_id"].as_str().expect("target_id").to_string();
    let r = c
        .ok(
            "browser_eval",
            json!({"target_id": tab, "expression": "document.hasFocus()"}),
        )
        .await;
    assert_eq!(r["result"], json!(true), "{r}");
    let _ = c
        .call(
            "browser_disconnect",
            json!({"browser_id": id, "kill": true}),
        )
        .await;
}
