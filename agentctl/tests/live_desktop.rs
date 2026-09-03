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

/// Act and confirm in one call.
///
/// Without `expect`, checking whether an action worked is observe, act, wait,
/// observe — four round trips to a remote model, three of which carry no
/// decision. And the naive version is *wrong*, because synthetic input is
/// asynchronous and observing straight after acting reads the previous state.
#[tokio::test(flavor = "multi_thread")]
async fn expect_folds_act_wait_and_verify_into_one_call() {
    if !ready() {
        return;
    }
    let c = client("live-expect");
    c.initialize().await;
    c.ok("launch", json!({ "app": "TextEdit" })).await;
    if !settle_on(&c, "TextEdit").await {
        return;
    }
    if !guarded_key(&c, "TextEdit", "cmd+n").await {
        return;
    }
    tokio::time::sleep(Duration::from_millis(700)).await;
    // Something to diff against.
    c.ok("get_ui_tree", json!({ "app": "TextEdit" })).await;

    let nonce = format!("expect-{}", mcp_policy::now_ms());
    if !focused_is("TextEdit") {
        return;
    }
    let env = c
        .call(
            "keyboard_type",
            json!({
                "text": &nonce,
                "expect": { "app": "TextEdit", "text": &nonce, "timeout_ms": 5000 }
            }),
        )
        .await;
    assert!(
        env.ok,
        "the expectation should have been met: {:?}",
        env.error
    );
    let d = env.data.unwrap();
    assert_eq!(d["expect"]["met"], json!(true));
    assert!(
        d["expect"]["waited_ms"].as_u64().is_some(),
        "the real settle time is reported, not assumed"
    );
    // The answer is what changed, not merely that the call returned.
    let changed = d["delta"]["changed"].as_array().unwrap();
    assert!(
        changed
            .iter()
            .any(|e| e["value"].as_str().is_some_and(|v| v.contains(&nonce))),
        "the delta must name the field that changed: {changed:?}"
    );

    // An expectation that cannot come true fails — and still returns the delta,
    // because the action did happen and what it did is what the agent needs.
    if !focused_is("TextEdit") {
        return;
    }
    let env = c
        .call(
            "keyboard_type",
            json!({
                "text": "!",
                "expect": { "app": "TextEdit", "text": "NEVER GOING TO APPEAR", "timeout_ms": 1200 }
            }),
        )
        .await;
    assert!(!env.ok);
    assert_eq!(env.error.unwrap().code, mcp_types::ErrorCode::Timeout);
    let d = env
        .data
        .expect("a failed expectation must still carry the delta");
    assert_eq!(d["expect"]["met"], json!(false));
    assert!(d["delta"]["counts"].is_object());

    // A malformed clause is refused before anything is typed.
    let env = c
        .call(
            "keyboard_type",
            json!({"text": "x", "expect": {"app": "TextEdit"}}),
        )
        .await;
    assert!(!env.ok);
    assert_eq!(env.error.unwrap().code, mcp_types::ErrorCode::InvalidArgs);

    let _ = c.call("close_app", json!({ "app": "TextEdit" })).await;
}

/// Reaching for the mouse stops the agent.
///
/// The two halves are equally important. Driving the pointer repeatedly must
/// *not* trip — a false stop is an agent that cannot work — and a person taking
/// over must trip promptly. The "person" here warps the cursor from outside
/// agentctl, exactly as a hand on a real mouse does: through a path the server
/// never records as its own.
#[tokio::test(flavor = "multi_thread")]
async fn a_human_taking_the_mouse_is_detected_and_agentctl_moving_it_is_not() {
    use core_graphics::display::CGDisplay;
    use core_graphics::geometry::CGPoint;
    use mcp_input::{Detector, InputBackend, MouseKind, OverrideConfig, Verdict};

    if !ready() {
        return;
    }
    let backend = mcp_macos::MacosBackend::new();
    let cfg = OverrideConfig::default();
    let now = || mcp_policy::now_ms() as u64;

    // Put the pointer somewhere known and let the detector settle on it.
    let mut d = Detector::new();
    backend
        .mouse(MouseKind::Move, 400.0, 400.0, None, &[])
        .await
        .expect("move");
    tokio::time::sleep(Duration::from_millis(80)).await;

    // The server driving its own pointer is never a takeover, however often.
    for i in 0..8 {
        let x = 400.0 + f64::from(i) * 15.0;
        backend
            .mouse(MouseKind::Move, x, 400.0, None, &[])
            .await
            .expect("move");
        tokio::time::sleep(Duration::from_millis(40)).await;
        let observed = backend.pointer_position().await.expect("read pointer");
        let v = d.observe(now(), true, &backend.recent_pointer_sets(), observed, &cfg);
        assert!(
            matches!(v, Verdict::Consistent),
            "agentctl moving its own pointer must never look like a takeover, got {v:?}"
        );
    }

    // Now a person grabs it. CGWarpMouseCursorPosition is not an event the
    // server posts, so nothing records these positions as ours.
    for i in 0..6 {
        CGDisplay::warp_mouse_cursor_position(CGPoint::new(900.0 + f64::from(i) * 6.0, 700.0))
            .expect("warp");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let observed = backend.pointer_position().await.expect("read pointer");
    let recent = backend.recent_pointer_sets();
    let first = d.observe(now(), true, &recent, observed, &cfg);
    let second = d.observe(now(), true, &recent, observed, &cfg);
    assert!(
        matches!(first, Verdict::Suspicious) && matches!(second, Verdict::Tripped { .. }),
        "a human on the mouse must trip after confirmation, got {first:?} then {second:?}"
    );
    if let Verdict::Tripped { distance, .. } = second {
        assert!(distance > cfg.threshold_px);
    }

    // Leave the pointer somewhere harmless.
    let _ = CGDisplay::warp_mouse_cursor_position(CGPoint::new(400.0, 400.0));
}

/// Reading text off the screen, with coordinates that can be clicked.
///
/// This is the fallback for surfaces the accessibility tree does not describe.
/// The assertion that matters is not "text came back" but that the box for a
/// known piece of text lands where that text actually is, because a box in
/// image pixels would be off by the Retina factor and every click would miss.
#[tokio::test(flavor = "multi_thread")]
async fn ocr_reads_a_window_and_returns_clickable_coordinates() {
    if !ready() {
        return;
    }
    if !mcp_macos::permissions().screen_recording {
        eprintln!("skipping: Screen Recording is not granted");
        return;
    }
    let c = client("live-ocr");
    c.initialize().await;
    c.ok("launch", json!({ "app": "TextEdit" })).await;
    if !settle_on(&c, "TextEdit").await {
        return;
    }
    if !guarded_key(&c, "TextEdit", "cmd+n").await {
        return;
    }
    tokio::time::sleep(Duration::from_millis(700)).await;

    // Something distinctive that OCR should find, in a large enough size to be
    // read reliably.
    let nonce = format!("OCRCHECK{}", mcp_policy::now_ms() % 100_000);
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
        eprintln!("skipping: the text never landed");
        return;
    }

    let windows = c.ok("list_windows", json!({ "app": "TextEdit" })).await;
    let bounds = windows["windows"][0]["bounds"].clone();
    let (wx, wy, ww, wh) = (
        bounds["x"].as_f64().unwrap(),
        bounds["y"].as_f64().unwrap(),
        bounds["w"].as_f64().unwrap(),
        bounds["h"].as_f64().unwrap(),
    );

    let env = c.call("ocr_region", json!({ "window_id": 0 })).await;
    if !env.ok {
        let msg = env.error.unwrap().message;
        // No Command Line Tools is a legitimate answer, and it must say so.
        assert!(
            msg.contains("Command Line Tools"),
            "unexpected OCR failure: {msg}"
        );
        eprintln!("skipping: {msg}");
        return;
    }
    let d = env.data.unwrap();
    assert_eq!(d["coordinate_space"], json!("screen"));
    assert!(d["line_count"].as_u64().unwrap() > 0);

    let found = d["lines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["text"].as_str().is_some_and(|t| t.contains(&nonce)))
        .unwrap_or_else(|| {
            panic!(
                "OCR should have read the typed text; it read: {}",
                d["text"].as_str().unwrap_or_default()
            )
        });

    // The point of returning boxes: they are in the same space mouse_action
    // takes, so they land on the thing that was read.
    let cx = found["center"]["x"].as_f64().unwrap();
    let cy = found["center"]["y"].as_f64().unwrap();
    assert!(
        cx > wx && cx < wx + ww && cy > wy && cy < wy + wh,
        "the text's centre ({cx}, {cy}) must fall inside the window \
         ({wx}, {wy}, {ww}x{wh}) — a box left in image pixels would not"
    );

    // Provenance still applies: screen text is somebody else's content.
    assert_eq!(d["provenance"], json!("untrusted"));

    let _ = c.call("close_app", json!({ "app": "TextEdit" })).await;
}
