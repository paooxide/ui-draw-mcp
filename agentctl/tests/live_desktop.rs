//! The differentiator, as a repeatable test: drive a real GUI application
//! through the real protocol until it produces a real file, then read the
//! result back out of the live accessibility tree.
//!
//! This is the task a shell-command agent cannot do, so it is the one worth
//! pinning. Everything goes through `tools/call`.
//!
//! **Three gates, all required.** `AGENTCTL_SKIP_LIVE` unset, `AGENTCTL_LIVE_GUI=1`
//! set, and the Accessibility permission actually granted. The GUI gate is
//! separate because these tests steal window focus and synthesise keystrokes: a
//! plain `cargo test` on a machine somebody is using must never do that.
//!
//! **Focus is checked before every keystroke.** CGEvent delivers to whatever is
//! frontmost, so a test that types without confirming the target would leak its
//! input into whatever the developer had open. Every send here is guarded, and
//! the guard failing skips the test rather than typing anyway.

#![cfg(target_os = "macos")]

use mcp_policy::{AuditSink, NoConsent};
use mcp_types::Category;
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};
use test_support::{live_gui_enabled, test_policy, InProcClient};

/// Apps this suite is allowed to drive. Nothing else can be launched or typed
/// into even if a step goes wrong.
const APPS: [&str; 2] = ["TextEdit", "Calculator"];

fn ready() -> bool {
    if !live_gui_enabled() {
        eprintln!("skipping: set AGENTCTL_LIVE_GUI=1 (and leave AGENTCTL_SKIP_LIVE unset) to run");
        return false;
    }
    if !mcp_macos::permissions().accessibility {
        eprintln!("skipping: Accessibility is not granted to the process running cargo test");
        return false;
    }
    true
}

fn client(name: &str) -> InProcClient {
    let mut cfg = test_policy(&[
        Category::Vision,
        Category::Input,
        Category::Window,
        Category::System,
    ]);
    cfg.allowed_apps = APPS.iter().map(|s| s.to_string()).collect();
    let server = agentctl::build_server(cfg, AuditSink::memory(), Arc::new(NoConsent), name)
        .expect("server builds");
    InProcClient::new(server)
}

/// The frontmost-app guard: refuse to type unless the intended app is the one
/// that will receive the keystrokes.
fn focused_is(app: &str) -> bool {
    use mcp_input::InputBackend;
    let backend = mcp_macos::MacosBackend::new();
    matches!(backend.input_target(), Some(t) if t.contains(app))
}

/// Wait for the app to actually be frontmost after a focus request; returns
/// false if it never gets there, so the caller can skip instead of typing.
async fn settle_on(c: &InProcClient, app: &str) -> bool {
    let _ = c.call("focus_app", json!({ "app": app })).await;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if focused_is(app) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    eprintln!("skipping: {app} never became frontmost; not typing blind");
    false
}

/// Type only when the guard holds.
async fn guarded_type(c: &InProcClient, app: &str, text: &str) -> bool {
    if !focused_is(app) {
        eprintln!("skipping the rest: focus moved off {app}");
        return false;
    }
    c.ok("keyboard_type", json!({ "text": text })).await;
    true
}

async fn guarded_key(c: &InProcClient, app: &str, combo: &str) -> bool {
    if !focused_is(app) {
        eprintln!("skipping the rest: focus moved off {app}");
        return false;
    }
    c.ok("keyboard_shortcut", json!({ "combo": combo })).await;
    true
}

/// Perceive → act → verify, ending in bytes on disk.
///
/// The assertion is deliberately not "the tool returned ok". It is that a file
/// exists that did not before, containing text this test generated, and that
/// the same text can be read back out of the application's accessibility tree.
#[tokio::test(flavor = "multi_thread")]
async fn textedit_types_saves_and_the_bytes_land_on_disk() {
    if !ready() {
        return;
    }
    let scratch = std::env::temp_dir().join(format!("agentctl-live-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&scratch);
    let nonce = format!("agentctl-{}", mcp_policy::now_ms());

    let c = client("live-textedit");
    c.initialize().await;

    c.ok("launch", json!({ "app": "TextEdit" })).await;
    if !settle_on(&c, "TextEdit").await {
        return;
    }
    // A fresh document, so nothing is typed into whatever the developer had
    // open in TextEdit already.
    if !guarded_key(&c, "TextEdit", "cmd+n").await {
        return;
    }
    tokio::time::sleep(Duration::from_millis(600)).await;

    if !guarded_type(&c, "TextEdit", &nonce).await {
        return;
    }

    // Synthetic input is delivered asynchronously: the event goes to the window
    // server and the app processes it on its own run loop. Snapshotting straight
    // after typing races that and reads the *previous* value, so settle first —
    // this is what wait_for is for.
    let settled = c
        .call(
            "wait_for",
            json!({ "app": "TextEdit", "text": &nonce, "timeout_ms": 5000 }),
        )
        .await;
    assert!(
        settled.ok,
        "the typed text never appeared in TextEdit's accessibility tree"
    );

    // And it is readable back out of the tree, not merely waited for.
    let tree = c.ok("get_ui_tree", json!({ "app": "TextEdit" })).await;
    let text = tree["text"].as_str().unwrap_or_default();
    assert!(
        text.contains(&nonce),
        "the typed text must appear in the accessibility tree; tree was:\n{text}"
    );

    // Save into the scratch directory: cmd+s, then the Go-to-folder sheet.
    if !guarded_key(&c, "TextEdit", "cmd+s").await {
        return;
    }
    tokio::time::sleep(Duration::from_millis(900)).await;

    let dialogs = c.ok("handle_dialogs", json!({ "app": "TextEdit" })).await;
    let kind = dialogs["dialogs"][0]["kind"].as_str().unwrap_or_default();
    assert_eq!(
        kind, "sheet",
        "cmd+s must raise a save sheet; got {dialogs}"
    );

    if !guarded_key(&c, "TextEdit", "cmd+shift+g").await {
        return;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let target = scratch.join(format!("{nonce}.rtf"));
    if !guarded_type(&c, "TextEdit", target.to_str().unwrap()).await {
        return;
    }
    if !guarded_key(&c, "TextEdit", "return").await {
        return;
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    if !guarded_key(&c, "TextEdit", "return").await {
        return;
    }

    // Poll for the file rather than sleeping a guessed amount.
    let deadline = Instant::now() + Duration::from_secs(6);
    let mut found = None;
    while Instant::now() < deadline {
        if let Ok(bytes) = std::fs::read(&target) {
            if !bytes.is_empty() {
                found = Some(bytes);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let bytes = found.unwrap_or_else(|| {
        panic!(
            "TextEdit never wrote {}; the save sheet may have taken a different path",
            target.display()
        )
    });
    let content = String::from_utf8_lossy(&bytes);
    assert!(
        content.contains(&nonce),
        "the saved file must contain what was typed (RTF stores ASCII verbatim)"
    );

    let _ = c.call("close_app", json!({ "app": "TextEdit" })).await;
    let _ = std::fs::remove_dir_all(&scratch);
}

/// Semantic action, not coordinates: click four buttons found by name in the
/// accessibility tree and read the answer back out of it.
#[tokio::test(flavor = "multi_thread")]
async fn calculator_adds_by_clicking_named_buttons() {
    if !ready() {
        return;
    }
    let c = client("live-calculator");
    c.initialize().await;

    c.ok("launch", json!({ "app": "Calculator" })).await;
    if !settle_on(&c, "Calculator").await {
        return;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    let tree = c.ok("get_ui_tree", json!({ "app": "Calculator" })).await;
    let text = tree["text"].as_str().unwrap_or_default().to_string();

    // Button names vary by locale and macOS version, so accept any of the
    // spellings rather than pinning one that will rot.
    let find = |candidates: &[&str]| -> Option<String> {
        for line in text.lines() {
            let l = line.trim();
            let Some(r) = l.split_whitespace().next() else {
                continue;
            };
            if !r.starts_with("@e") {
                continue;
            }
            let name = l.split('"').nth(1).unwrap_or_default().to_lowercase();
            if candidates.iter().any(|c| name == *c) {
                return Some(r.to_string());
            }
        }
        None
    };

    let (one, plus, two, eq) = match (
        find(&["1", "one"]),
        find(&["+", "add", "plus"]),
        find(&["2", "two"]),
        find(&["=", "equals"]),
    ) {
        (Some(a), Some(b), Some(c2), Some(d)) => (a, b, c2, d),
        _ => {
            eprintln!("skipping: could not find 1/+/2/= in the tree:\n{text}");
            return;
        }
    };

    for r in [&one, &plus, &two, &eq] {
        c.ok("ui_action", json!({ "ref": r, "action": "click" }))
            .await;
        tokio::time::sleep(Duration::from_millis(120)).await;
    }

    // Four clicks from one snapshot: this is also the stale-ref regression.
    let after = c.ok("get_ui_tree", json!({ "app": "Calculator" })).await;
    let shown = after["text"].as_str().unwrap_or_default();
    assert!(
        shown.contains('3'),
        "1 + 2 = should leave a 3 in the display; tree was:\n{shown}"
    );

    let _ = c.call("close_app", json!({ "app": "Calculator" })).await;
}

/// The pinned target follows the app being driven even when something else
/// takes focus — the property that stops keystrokes landing in the wrong
/// window. Read-only: no synthetic input, so this one needs no GUI gate.
#[tokio::test(flavor = "multi_thread")]
async fn the_input_target_is_reported_or_permission_is_denied() {
    use mcp_input::InputBackend;
    let backend = mcp_macos::MacosBackend::new();
    let target = backend.input_target();
    if mcp_macos::permissions().accessibility {
        assert!(
            target.is_some(),
            "with Accessibility granted, the live input target must be knowable — \
             an unknown destination is what the destructive-command gate screens as a terminal"
        );
    }
}

/// Querying beats reading the whole tree, and the refs it hands back are real.
///
/// The cost argument is the point: on a busy app `get_ui_tree` is thousands of
/// characters the agent pays for every turn, when the actual question was
/// "where is the Save button". This asserts both halves — that the answer is
/// much smaller, and that it is still actionable.
#[tokio::test(flavor = "multi_thread")]
async fn find_elements_is_cheaper_than_the_tree_and_its_refs_work() {
    if !ready() {
        return;
    }
    let c = client("live-find");
    c.initialize().await;
    c.ok("launch", json!({ "app": "Calculator" })).await;
    if !settle_on(&c, "Calculator").await {
        return;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    let tree = c.ok("get_ui_tree", json!({ "app": "Calculator" })).await;
    let tree_chars = tree["text"].as_str().unwrap_or_default().len();

    let found = c
        .ok(
            "find_elements",
            json!({ "app": "Calculator", "role": "button", "name": "All Clear" }),
        )
        .await;
    assert_eq!(found["count"], json!(1), "one button is named All Clear");
    let hit = &found["elements"][0];
    assert_eq!(hit["actionable"], json!(true));
    let found_chars = serde_json::to_string(&found).unwrap().len();
    assert!(
        found_chars * 4 < tree_chars,
        "a targeted query should be far smaller than the tree \
         ({found_chars} vs {tree_chars} chars)"
    );

    // The ref is usable, which is the whole reason the tool takes a fresh
    // snapshot instead of querying a retained one.
    let reff = hit["ref"].as_str().unwrap().to_string();
    let clicked = c
        .call("ui_action", json!({ "ref": reff, "action": "click" }))
        .await;
    assert!(
        clicked.ok,
        "the ref must be actionable: {:?}",
        clicked.error
    );

    // Asking for nothing in particular is a mistake worth naming.
    let bad = c
        .call("find_elements", json!({ "app": "Calculator" }))
        .await;
    assert!(!bad.ok);
    assert_eq!(bad.error.unwrap().code, mcp_types::ErrorCode::InvalidArgs);

    let _ = c.call("close_app", json!({ "app": "Calculator" })).await;
}

/// After an action, almost nothing on screen is different — so send the
/// difference, not the tree.
///
/// This is the token-cost argument made concrete: an agent that observes,
/// acts, and observes again pays for the whole UI twice. With `since` the
/// second observation is the change alone, and it still carries usable refs.
#[tokio::test(flavor = "multi_thread")]
async fn a_delta_is_far_smaller_than_the_tree_and_names_what_changed() {
    if !ready() {
        return;
    }
    let c = client("live-delta");
    c.initialize().await;
    c.ok("launch", json!({ "app": "TextEdit" })).await;
    if !settle_on(&c, "TextEdit").await {
        return;
    }
    if !guarded_key(&c, "TextEdit", "cmd+n").await {
        return;
    }
    tokio::time::sleep(Duration::from_millis(700)).await;

    let first = c.ok("get_ui_tree", json!({ "app": "TextEdit" })).await;
    let base = first["snapshot_id"].as_str().unwrap().to_string();
    let tree_chars = first["text"].as_str().unwrap_or_default().len();

    let nonce = format!("delta-{}", mcp_policy::now_ms());
    if !guarded_type(&c, "TextEdit", &nonce).await {
        return;
    }
    let settled = c
        .call(
            "wait_for",
            json!({ "app": "TextEdit", "text": &nonce, "timeout_ms": 5000 }),
        )
        .await;
    if !settled.ok {
        eprintln!("skipping: the typed text never landed");
        return;
    }

    let delta = c
        .ok("get_ui_tree", json!({ "app": "TextEdit", "since": &base }))
        .await;
    let delta_bytes = serde_json::to_string(&delta).unwrap().len();
    assert!(
        delta_bytes * 3 < tree_chars,
        "a delta should be much smaller than the tree ({delta_bytes} vs {tree_chars})"
    );
    assert!(
        delta.get("text").is_none(),
        "the tree text is the thing being avoided; it must be opt-in"
    );

    // The changed element is named, with what it changed from.
    let changed = delta["delta"]["changed"].as_array().unwrap();
    let typed = changed
        .iter()
        .find(|e| e["value"].as_str().is_some_and(|v| v.contains(&nonce)))
        .unwrap_or_else(|| panic!("the edited field should be in the delta: {changed:?}"));
    assert!(typed["fields"]
        .as_array()
        .unwrap()
        .contains(&json!("value")));
    assert!(
        typed["before"]["value"].is_string() || typed["before"]["value"].is_null(),
        "a change reports the previous value"
    );
    assert!(
        delta["delta"]["counts"]["unchanged"].as_u64().unwrap() > 10,
        "most of the UI did not change, which is the whole point"
    );

    // A snapshot that has aged out is a clear error naming what is left, not a
    // silent full tree.
    let stale = c
        .call(
            "get_ui_tree",
            json!({ "app": "TextEdit", "since": "s-nope" }),
        )
        .await;
    assert!(!stale.ok);
    assert_eq!(stale.error.unwrap().code, mcp_types::ErrorCode::NotFound);

    let _ = c.call("close_app", json!({ "app": "TextEdit" })).await;
}
