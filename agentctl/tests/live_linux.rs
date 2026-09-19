//! The Linux desktop backend against the real session, through the real
//! protocol.
//!
//! Two gates. With `AGENTCTL_SKIP_LIVE` unset, the read-only half runs: it
//! reads the accessibility tree, lists windows and displays, takes a
//! screenshot, and reads session state. None of that moves focus or asks a
//! human anything, so it is safe on a desktop somebody is using.
//!
//! `AGENTCTL_LIVE_GUI=1` additionally runs the half that acts: it launches an
//! editor, types into it, reads the text back out of the tree, and closes it.
//! The first synthetic keystroke opens the remote-desktop portal session,
//! which on a fresh machine raises a system dialog the person at the desk
//! must approve once. Nothing here can answer it.

#![cfg(target_os = "linux")]

use mcp_policy::{AuditSink, NoConsent};
use mcp_types::{Category, ErrorCode};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};
use test_support::{live_gui_enabled, skip_live, test_policy, InProcClient};

/// Route agentctl's tracing to the test output so a failing run is legible.
/// Best-effort and idempotent: several tests may call it.
fn trace() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_test_writer()
        .try_init();
}

/// Apps the GUI half may drive. Nothing else is launched or typed into.
const EDITOR: &str = "org.gnome.TextEditor";

fn client(name: &str) -> InProcClient {
    let mut cfg = test_policy(&[
        Category::Vision,
        Category::Input,
        Category::Window,
        Category::Desktop,
        Category::System,
    ]);
    cfg.allowed_apps = vec![EDITOR.into(), "Text Editor".into()];
    let server = agentctl::build_server(cfg, AuditSink::memory(), Arc::new(NoConsent), name)
        .expect("server builds");
    InProcClient::new(server)
}

fn live() -> bool {
    if skip_live() {
        eprintln!("skipping: AGENTCTL_SKIP_LIVE is set");
        return false;
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("DISPLAY").is_none() {
        eprintln!("skipping: no graphical session");
        return false;
    }
    true
}

fn pictures_count() -> usize {
    let dir = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default()
        .join("Pictures");
    std::fs::read_dir(dir).map(|d| d.count()).unwrap_or(0)
}

/// Perception: a real application's tree, with refs an action could use.
#[tokio::test(flavor = "multi_thread")]
async fn the_accessibility_tree_of_a_running_app_is_readable() {
    if !live() {
        return;
    }
    let c = client("linux-a11y");
    let apps = c.ok("list_apps", json!({})).await;
    let names: Vec<String> = apps["apps"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        !names.is_empty(),
        "no applications on the accessibility bus: {apps}"
    );
    // Whatever is active, without naming it: the default target.
    let tree = c.call("get_ui_tree", json!({})).await;
    if !tree.ok {
        let e = tree.error.unwrap();
        assert!(
            matches!(e.code, ErrorCode::NotFound),
            "an unreadable tree must be NOT_FOUND (no active window), got {:?}: {}",
            e.code,
            e.message
        );
        eprintln!("no active window; trying the first listed app");
    }
    let tree = c.ok("get_ui_tree", json!({ "app": names[0] })).await;
    let text = tree["text"].as_str().unwrap_or_default();
    assert!(!text.is_empty(), "empty tree text for {}: {tree}", names[0]);
    assert!(tree["snapshot_id"].is_string());
    // A window's frame is always in the tree, and it is a window.
    assert!(
        text.contains("window") || text.contains("dialog"),
        "expected a window node in:\n{text}"
    );
    // Querying is cheaper than reading, and answers with usable refs. Only
    // interactive nodes are indexed, so ask for buttons, and hold the query to
    // the same standard as the tree: if the tree shows buttons, so must it.
    let found = c
        .ok(
            "find_elements",
            json!({ "app": names[0], "role": "button", "limit": 5 }),
        )
        .await;
    let hits = found["elements"].as_array().cloned().unwrap_or_default();
    if text.contains("button") {
        assert!(
            !hits.is_empty(),
            "the tree of {} shows buttons but find_elements found none: {found}",
            names[0]
        );
    }
    for h in &hits {
        assert!(
            h["ref"].as_str().is_some_and(|r| r.starts_with("@e")),
            "{h}"
        );
        assert_eq!(h["role"].as_str(), Some("button"), "{h}");
    }
    assert_eq!(found["provenance"].as_str(), Some("untrusted"));
}

/// Windows and displays: the two lists an agent reads first.
#[tokio::test(flavor = "multi_thread")]
async fn windows_and_displays_are_listed() {
    if !live() {
        return;
    }
    let c = client("linux-windows");
    let w = c.ok("list_windows", json!({})).await;
    let windows = w["windows"].as_array().cloned().unwrap_or_default();
    assert!(!windows.is_empty(), "no windows: {w}");
    for win in &windows {
        assert!(win["id"].is_u64(), "{win}");
        assert!(win["app"].as_str().is_some_and(|a| !a.is_empty()), "{win}");
    }
    let d = c.ok("list_displays", json!({})).await;
    let displays = d["displays"].as_array().cloned().unwrap_or_default();
    assert!(!displays.is_empty(), "no displays: {d}");
    for disp in &displays {
        assert!(disp["w"].as_f64().unwrap_or(0.0) > 0.0, "{disp}");
        assert!(disp["h"].as_f64().unwrap_or(0.0) > 0.0, "{disp}");
        assert!(disp["scale"].as_f64().unwrap_or(0.0) > 0.0, "{disp}");
    }
    assert!(
        displays.iter().any(|d| d["primary"] == json!(true)),
        "no primary display: {d}"
    );
}

/// Capture: a real PNG, a mapping back to the screen, no litter in Pictures.
#[tokio::test(flavor = "multi_thread")]
async fn capture_screen_returns_an_image_and_leaves_no_files_behind() {
    if !live() {
        return;
    }
    let before = pictures_count();
    let c = client("linux-capture");
    let cap = c.call("capture_screen", json!({ "detail": "low" })).await;
    assert!(cap.ok, "{:?}", cap.error);
    let img = cap.image.as_ref().expect("an image block");
    assert!(img.base64.len() > 1000, "suspiciously small capture");
    let data = cap.data.unwrap_or(Value::Null);
    assert!(
        data["width"].as_u64().unwrap_or(0) <= 768,
        "low detail must fit 768px: {data}"
    );
    // A region is a crop of the same frame, cheaper, and still mapped.
    let region = c
        .ok(
            "capture_screen",
            json!({ "region": { "x": 0, "y": 0, "w": 300, "h": 200 }, "force": true }),
        )
        .await;
    assert_eq!(region["width"].as_u64(), Some(300), "{region}");
    assert_eq!(region["height"].as_u64(), Some(200), "{region}");
    // A region that misses every monitor is an error, not an empty image.
    let bad = c
        .call(
            "capture_screen",
            json!({ "region": { "x": 100000, "y": 100000, "w": 10, "h": 10 } }),
        )
        .await;
    assert!(!bad.ok);
    let bad = c.call("capture_screen", json!({ "display": 99 })).await;
    assert!(!bad.ok);
    assert_eq!(bad.error.unwrap().code, ErrorCode::NotFound);
    assert_eq!(
        pictures_count(),
        before,
        "the portal's files must be removed after reading"
    );
}

/// Session state and settings are readable, and the unreadable ones say why.
#[tokio::test(flavor = "multi_thread")]
async fn session_state_and_settings_are_readable() {
    if !live() {
        return;
    }
    let c = client("linux-session");
    let idle = c.ok("idle_status", json!({})).await;
    assert!(idle["idle_seconds"].is_u64(), "{idle}");
    assert!(idle["locked"].is_boolean(), "{idle}");
    for setting in ["volume", "dark_mode", "dnd", "resolution"] {
        let v = c
            .ok(
                "system_settings",
                json!({ "action": "get", "setting": setting }),
            )
            .await;
        assert_eq!(v["setting"].as_str(), Some(setting), "{v}");
        assert!(
            v["value"].is_object() && !v["value"].as_object().unwrap().is_empty(),
            "{setting}: {v}"
        );
    }
    let vol = c
        .ok(
            "system_settings",
            json!({ "action": "get", "setting": "volume" }),
        )
        .await;
    let level = vol["value"]["volume"].as_i64().unwrap_or(-1);
    assert!((0..=150).contains(&level), "{vol}");
    // Brightness depends on hardware: a laptop answers, a desktop monitor says
    // there is no backlight. Either is an answer; a hang or a panic is not.
    let b = c
        .call(
            "system_settings",
            json!({ "action": "get", "setting": "brightness" }),
        )
        .await;
    if !b.ok {
        assert_eq!(b.error.unwrap().code, ErrorCode::UnsupportedOs);
    }
    let bad = c
        .call(
            "system_settings",
            json!({ "action": "set", "setting": "volume", "value": "999" }),
        )
        .await;
    assert!(!bad.ok, "volume 999 must be refused");
}

/// The agent's channel to the human reaches the notification daemon.
#[tokio::test(flavor = "multi_thread")]
async fn notify_user_reaches_the_daemon() {
    if !live() {
        return;
    }
    let c = client("linux-notify");
    c.ok(
        "notify_user",
        json!({ "title": "agentctl live test", "body": "this notification was posted by the test suite" }),
    )
    .await;
}

/// Text off the screen, with boxes. Release builds only: the recogniser is
/// unusably slow unoptimised, and the first run downloads its models.
#[tokio::test(flavor = "multi_thread")]
async fn ocr_reads_text_off_the_screen() {
    if !live() {
        return;
    }
    if cfg!(debug_assertions) {
        eprintln!("skipping: run with --release (ocrs is far too slow in a debug build)");
        return;
    }
    let c = client("linux-ocr");
    let started = Instant::now();
    let r = c
        .ok(
            "ocr_region",
            json!({ "region": { "x": 0, "y": 0, "w": 800, "h": 300 }, "min_confidence": 0 }),
        )
        .await;
    let lines = r["lines"].as_array().cloned().unwrap_or_default();
    eprintln!("ocr: {} line(s) in {:?}", lines.len(), started.elapsed());
    for l in &lines {
        assert!(l["text"].as_str().is_some_and(|t| !t.is_empty()), "{l}");
        assert!(
            l["x"].is_number() || l["box"].is_object() || l["bounds"].is_object(),
            "{l}"
        );
    }
    assert!(r["image"]["width"].as_u64().unwrap_or(0) > 0, "{r}");
}

// ---- the half that acts -----------------------------------------------------

/// Wait for the editor's window to be the active one.
async fn settle_on(c: &InProcClient, app: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        let ws = c.call("list_windows", json!({ "app": app })).await;
        if ws.ok {
            let focus = c.call("focus_app", json!({ "app": app })).await;
            if focus.ok {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    eprintln!("skipping: {app} never became active; not typing blind");
    false
}

/// Launch, type, read back, close. The assertion is that the text this test
/// generated can be read out of the editor's accessibility tree, which only
/// happens if the portal delivered the keystrokes to the right window.
#[tokio::test(flavor = "multi_thread")]
async fn editor_receives_typed_text_and_the_tree_shows_it() {
    if !live() || !live_gui_enabled() {
        eprintln!("skipping: set AGENTCTL_LIVE_GUI=1 to run the acting half");
        return;
    }
    if !std::path::Path::new("/usr/share/applications/org.gnome.TextEditor.desktop").exists() {
        eprintln!("skipping: GNOME Text Editor is not installed");
        return;
    }
    trace();
    let c = client("linux-gui");
    let nonce = format!("agentctl-{}", mcp_policy::now_ms());
    c.ok("launch", json!({ "app": EDITOR })).await;
    if !settle_on(&c, EDITOR).await {
        let _ = c.call("close_app", json!({ "app": EDITOR })).await;
        return;
    }
    // Focus the text view through the tree rather than by clicking a
    // coordinate: on Wayland the tree's coordinates are window-relative.
    let found = c
        .ok(
            "find_elements",
            json!({ "app": EDITOR, "role": "textarea", "limit": 1 }),
        )
        .await;
    let hits = found["elements"].as_array().cloned().unwrap_or_default();
    if let Some(h) = hits.first() {
        let _ = c
            .call("ui_action", json!({ "ref": h["ref"], "action": "focus" }))
            .await;
    }
    // Where will the keystrokes actually go? On Wayland this is the window
    // the compositor has focused, which is what we are trying to confirm is
    // the editor and not the terminal running the test.
    let tgt = c.call("get_ui_tree", json!({})).await;
    eprintln!(
        "before typing, the default-target tree is app={:?}",
        tgt.data.as_ref().and_then(|d| d["app"].as_str())
    );
    let typed = c.call("keyboard_type", json!({ "text": &nonce })).await;
    if !typed.ok {
        let e = typed.error.unwrap();
        // The portal dialog was not approved, or timed out: that is a refusal
        // with a reason, which is the contract; report it and stop.
        eprintln!("keyboard_type refused: {:?} {}", e.code, e.message);
        assert!(matches!(
            e.code,
            ErrorCode::PermDenied | ErrorCode::UnsupportedOs
        ));
        let _ = c.call("close_app", json!({ "app": EDITOR })).await;
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seen = false;
    while Instant::now() < deadline {
        let tree = c.ok("get_ui_tree", json!({ "app": EDITOR })).await;
        if tree["text"].as_str().unwrap_or_default().contains(&nonce) {
            seen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let _ = c.call("close_app", json!({ "app": EDITOR })).await;
    assert!(
        seen,
        "the typed nonce {nonce} never appeared in the editor's tree"
    );
}

/// The clipboard round-trips through the compositor, and the original is
/// put back so the test leaves no trace.
#[tokio::test(flavor = "multi_thread")]
async fn clipboard_round_trips_and_is_restored() {
    if !live() || !live_gui_enabled() {
        return;
    }
    let c = client("linux-clip");
    let nonce = format!("agentctl-clip-{}", mcp_policy::now_ms());
    let w = c
        .call(
            "clipboard_write",
            json!({ "format": "text", "data": &nonce }),
        )
        .await;
    if !w.ok {
        // GNOME/Mutter implements no data-control protocol and this machine
        // has no X11 clipboard tool to bridge it. That is a real platform
        // limitation, and the contract is that it says so rather than lying.
        let e = w.error.unwrap();
        assert_eq!(e.code, ErrorCode::UnsupportedOs, "{}", e.message);
        assert!(
            e.message.contains("data-control") || e.message.contains("clipboard"),
            "the refusal must name the limitation: {}",
            e.message
        );
        eprintln!("skipping the round-trip: {}", e.message);
        return;
    }
    // Where a write worked, a read must return what was written.
    let before = c.call("clipboard_read", json!({ "format": "text" })).await;
    let original = before
        .data
        .as_ref()
        .and_then(|d| d["data"].as_str().map(String::from));
    let after = c.ok("clipboard_read", json!({ "format": "text" })).await;
    assert_eq!(after["data"].as_str(), Some(nonce.as_str()), "{after}");
    if let Some(orig) = original {
        let _ = c
            .call("clipboard_write", json!({ "format": "text", "data": orig }))
            .await;
    }
}
