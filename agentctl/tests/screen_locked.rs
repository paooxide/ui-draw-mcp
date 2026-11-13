//! A locked screen through the real stack: the detector, the tool refusal and
//! `idle_status`.
//!
//! Passive. Nothing here needs the screen unlocked and nothing is posted while
//! it is locked, which is the point: with the lock screen up the system
//! discards synthetic key and pointer events while `CGEventPost` still
//! succeeds, so before this check agentctl reported the action as done. The
//! refusal test only calls the key and pointer tools when the session really
//! cannot take input, and reads the HID idle clock around them to show that
//! nothing was posted. Skipped under `AGENTCTL_SKIP_LIVE=1`.

#![cfg(target_os = "macos")]

use std::sync::Arc;

use mcp_policy::{AuditSink, NoConsent};
use mcp_types::{Category, ErrorCode, Foreground, ScreenState};
use serde_json::json;
use test_support::{skip_live, test_policy, InProcClient};

fn client(foreground: Foreground, name: &str) -> InProcClient {
    let mut cfg = test_policy(&[
        Category::Input,
        Category::Window,
        Category::Desktop,
        Category::Vision,
    ]);
    cfg.allowed_apps = vec!["Finder".to_string()];
    cfg.foreground = foreground;
    // Short, so a polite call that wrongly waited would be caught by time.
    cfg.polite_idle_ms = 60_000;
    cfg.polite_max_wait_ms = 60_000;
    let server = agentctl::build_server(cfg, AuditSink::memory(), Arc::new(NoConsent), name)
        .expect("server builds");
    InProcClient::new(server)
}

#[test]
fn the_session_state_is_a_definite_answer_on_this_machine() {
    if skip_live() {
        eprintln!("skipping: AGENTCTL_SKIP_LIVE is set");
        return;
    }
    let state = mcp_macos::screen_state();
    eprintln!("session state: {}", state.label());
    assert_ne!(
        state,
        ScreenState::Unknown,
        "a logged-in desktop session must say whether it is locked or on the console"
    );
    assert_eq!(state, mcp_macos::screen_state(), "stable across two reads");
}

#[tokio::test]
async fn idle_status_reports_the_same_state_the_refusal_uses() {
    if skip_live() {
        eprintln!("skipping: AGENTCTL_SKIP_LIVE is set");
        return;
    }
    let c = client(Foreground::Allow, "screen-idle");
    let state = mcp_macos::screen_state();
    let data = c.ok("idle_status", json!({})).await;
    assert_eq!(data["input_state"], json!(state.label()), "{data}");
    assert_eq!(
        data["screen_locked"],
        json!(state == ScreenState::Locked),
        "{data}"
    );
    assert_eq!(data["locked"], data["screen_locked"], "{data}");
}

#[tokio::test]
async fn while_the_session_cannot_take_input_every_posting_call_is_refused_and_posts_nothing() {
    if skip_live() {
        eprintln!("skipping: AGENTCTL_SKIP_LIVE is set");
        return;
    }
    let state = mcp_macos::screen_state();
    if !state.blocks_input() {
        eprintln!(
            "skipping: the session is {}, so there is nothing to refuse; run with the screen \
             locked to exercise this",
            state.label()
        );
        return;
    }
    for foreground in [Foreground::Allow, Foreground::Polite] {
        let c = client(foreground, "screen-refusal");
        let idle_before = c.ok("idle_status", json!({})).await["idle_seconds"]
            .as_u64()
            .expect("idle seconds");
        let started = std::time::Instant::now();
        let calls = [
            ("hover", json!({"x": 40, "y": 40})),
            ("mouse_action", json!({"type": "click", "x": 40, "y": 40})),
            (
                "scroll",
                json!({"x": 40, "y": 40, "direction": "down", "amount": 1}),
            ),
            ("keyboard_type", json!({"text": "x"})),
            ("keyboard_shortcut", json!({"combo": "shift"})),
            ("focus_app", json!({"app": "Finder"})),
        ];
        for (tool, args) in calls {
            let env = c.call(tool, args).await;
            let err = env
                .error
                .unwrap_or_else(|| panic!("{foreground:?} {tool} was reported as done"));
            eprintln!("{foreground:?} {tool}: {err:?}");
            assert_eq!(err.code, ErrorCode::ActionFailed, "{tool}: {err:?}");
            assert!(
                err.message.contains("nothing was done"),
                "{tool}: {}",
                err.message
            );
            assert!(
                err.suggestion
                    .as_deref()
                    .unwrap_or_default()
                    .starts_with("retryable"),
                "{tool}: {err:?}"
            );
        }
        // Polite must not have waited out its minute for the person to idle.
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "{foreground:?} waited before refusing"
        );
        // The HID clock is how the owner's own input shows: a posted event
        // would have reset it.
        let idle_after = c.ok("idle_status", json!({})).await["idle_seconds"]
            .as_u64()
            .expect("idle seconds");
        assert!(
            idle_after >= idle_before,
            "{foreground:?}: the idle clock went back from {idle_before} to {idle_after}: \
             something was posted"
        );
    }
}
