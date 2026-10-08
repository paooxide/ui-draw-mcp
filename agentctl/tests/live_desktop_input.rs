//! What the desktop input tools report must be what the application did.
//!
//! The mock-backed tests in `mcp-input` prove the tool layer judges a read-back
//! correctly. They cannot prove the read-back is true, or that a popup button
//! really takes a choice made by text; only a real application can. These
//! drive TextEdit, which every Mac has, and assert on what its accessibility
//! tree says afterwards rather than on what the tools returned.
//!
//! Gated exactly like `live_desktop.rs`: `AGENTCTL_SKIP_LIVE` unset,
//! `AGENTCTL_LIVE_GUI=1`, and the Accessibility permission granted. They steal
//! focus and synthesise keystrokes, so a plain `cargo test` never runs them.
//! Every keystroke is guarded on TextEdit being frontmost, every action is on a
//! new scratch document or a Save sheet that is cancelled, and nothing is saved.
//!
//! Which popup and which checkbox is not pinned. The Save sheet's controls
//! differ across macOS versions and settings, so the tests find a popup button
//! in the sheet, discover its options from the error a deliberately wrong
//! choice returns (which is itself under test), and skip, saying why, if the
//! sheet has none.

#![cfg(target_os = "macos")]

use mcp_policy::{AuditSink, NoConsent};
use mcp_types::{Category, ErrorCode};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};
use test_support::{live_gui_enabled, test_policy, InProcClient};

const APP: &str = "TextEdit";

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
    cfg.allowed_apps = vec![APP.to_string()];
    let server = agentctl::build_server(cfg, AuditSink::memory(), Arc::new(NoConsent), name)
        .expect("server builds");
    InProcClient::new(server)
}

fn focused_is_textedit() -> bool {
    use mcp_input::InputBackend;
    let backend = mcp_macos::MacosBackend::new();
    matches!(backend.input_target(), Some(t) if t.contains(APP))
}

async fn settle_on_textedit(c: &InProcClient) -> bool {
    let _ = c.call("focus_app", json!({ "app": APP })).await;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if focused_is_textedit() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    eprintln!("skipping: {APP} never became frontmost; not typing blind");
    false
}

/// A chord, only while TextEdit is the app that will receive it.
async fn guarded_key(c: &InProcClient, combo: &str) -> bool {
    if !focused_is_textedit() {
        eprintln!("skipping the rest: focus moved off {APP}");
        return false;
    }
    c.ok("keyboard_shortcut", json!({ "combo": combo })).await;
    true
}

/// The options a popup offers, taken from the error a wrong choice returns:
/// `no option "x"; the options are: "A", "B", ... (3 more)`.
fn listed_options(message: &str) -> Vec<String> {
    let Some((_, list)) = message.split_once("the options are: ") else {
        return Vec::new();
    };
    list.split('"')
        .enumerate()
        .filter(|(i, _)| i % 2 == 1)
        .map(|(_, s)| s.to_string())
        .collect()
}

/// A choice that would open another dialog or leave the sheet is not one a
/// test should make.
fn safe_to_choose(option: &str) -> bool {
    !(option.ends_with('…') || option.ends_with("...") || option.to_lowercase().contains("other"))
}

/// Open a new document's Save sheet. `None` if the app did not get there.
async fn open_save_sheet(c: &InProcClient) -> Option<()> {
    c.ok("launch", json!({ "app": APP })).await;
    if !settle_on_textedit(c).await {
        return None;
    }
    if !guarded_key(c, "cmd+n").await {
        return None;
    }
    tokio::time::sleep(Duration::from_millis(700)).await;
    if !guarded_key(c, "Cmd+S").await {
        return None;
    }
    tokio::time::sleep(Duration::from_millis(900)).await;
    let dialogs = c.ok("handle_dialogs", json!({ "app": APP })).await;
    if dialogs["dialogs"][0]["kind"].as_str() != Some("sheet") {
        eprintln!("skipping: cmd+s did not raise a save sheet; got {dialogs}");
        return None;
    }
    Some(())
}

async fn cancel_and_close(c: &InProcClient) {
    // Escape cancels the sheet. It is sent only while TextEdit is frontmost.
    if focused_is_textedit() {
        let _ = c
            .call("keyboard_shortcut", json!({ "combo": "Escape" }))
            .await;
    }
    let _ = c.call("close_app", json!({ "app": APP })).await;
}

/// Choose an entry of a real popup button by its text, and check the answer
/// against what the control shows afterwards.
///
/// The Save sheet has at least one popup button (Where, and the file format
/// when options are shown). A wrong option must list the real ones and leave
/// the menu closed; a right one must change what the control shows, and
/// choosing it again must say nothing changed.
#[tokio::test(flavor = "multi_thread")]
async fn a_popup_button_in_the_save_sheet_takes_a_choice_by_its_text() {
    if !ready() {
        return;
    }
    let c = client("live-popup");
    c.initialize().await;
    if open_save_sheet(&c).await.is_none() {
        return;
    }

    let found = c
        .ok(
            "find_elements",
            json!({ "app": APP, "surface": "sheet", "role": "popup button" }),
        )
        .await;
    let popups = found["elements"].as_array().cloned().unwrap_or_default();
    let Some(popup) = popups.first() else {
        eprintln!(
            "skipping: the save sheet exposes no popup button ({})",
            found["hint"]
        );
        cancel_and_close(&c).await;
        return;
    };
    let reff = popup["ref"].as_str().unwrap().to_string();

    // A choice that does not exist is refused, with the real ones listed, and
    // nothing is left open behind it.
    let wrong = c
        .call(
            "ui_action",
            json!({ "ref": &reff, "action": "select", "option": "agentctl-no-such-option" }),
        )
        .await;
    assert!(!wrong.ok, "a missing option must not report ok");
    let err = wrong.error.unwrap();
    assert_eq!(err.code, ErrorCode::InvalidArgs, "{}", err.message);
    let options = listed_options(&err.message);
    assert!(
        !options.is_empty(),
        "the error must list the options: {}",
        err.message
    );

    // Still operable: a menu left open would swallow or misroute the next call.
    let again = c
        .ok(
            "find_elements",
            json!({ "app": APP, "surface": "sheet", "role": "popup button" }),
        )
        .await;
    assert!(
        again["count"].as_u64().unwrap_or(0) >= 1,
        "the popup must still be there after a refused choice"
    );

    let current = again["elements"][0]["value"]
        .as_str()
        .or_else(|| again["elements"][0]["name"].as_str())
        .unwrap_or_default()
        .to_string();
    let Some(target) = options
        .iter()
        .find(|o| safe_to_choose(o) && !o.eq_ignore_ascii_case(&current))
    else {
        eprintln!("skipping: no other safe option to choose among {options:?}");
        cancel_and_close(&c).await;
        return;
    };

    // Case and spacing are forgiven; the answer is the control's own text.
    let reff = again["elements"][0]["ref"].as_str().unwrap().to_string();
    let chose = c
        .ok(
            "ui_action",
            json!({ "ref": &reff, "action": "select", "option": target.to_uppercase() }),
        )
        .await;
    assert_eq!(chose["selected"], json!(target), "{chose}");
    assert_eq!(chose["changed"], json!(true), "{chose}");

    // Believe the tree, not the tool: the control now shows the choice.
    let after = c
        .ok(
            "find_elements",
            json!({ "app": APP, "surface": "sheet", "role": "popup button" }),
        )
        .await;
    let shown = after["elements"][0]["value"]
        .as_str()
        .or_else(|| after["elements"][0]["name"].as_str())
        .unwrap_or_default();
    assert_eq!(shown, target, "the control must show what was chosen");

    // Choosing what is already chosen changes nothing, and says so. Named, not
    // by ref: the name of a popup is what it currently shows.
    let same = c
        .ok(
            "ui_action",
            json!({ "name": target, "role": "popup button", "action": "select", "option": target }),
        )
        .await;
    assert_eq!(same["changed"], json!(false), "{same}");
    assert!(same["ref"].is_string(), "the resolved ref is reported");

    // And the same call through ui_fill_form.
    let filled = c
        .ok(
            "ui_fill_form",
            json!({ "fields": [{ "name": target, "role": "popup button", "action": "select", "option": current }] }),
        )
        .await;
    assert_eq!(filled["results"][0]["selected"], json!(current), "{filled}");

    cancel_and_close(&c).await;
}

/// A checkbox is checked, not pressed: asking for the state it has changes
/// nothing, and the answer is read back.
#[tokio::test(flavor = "multi_thread")]
async fn a_checkbox_in_the_save_sheet_is_checked_idempotently_and_verified() {
    if !ready() {
        return;
    }
    let c = client("live-checkbox");
    c.initialize().await;
    if open_save_sheet(&c).await.is_none() {
        return;
    }
    let found = c
        .ok(
            "find_elements",
            json!({ "app": APP, "surface": "sheet", "role": "check box" }),
        )
        .await;
    let Some(boxes) = found["elements"].as_array().filter(|b| !b.is_empty()) else {
        eprintln!(
            "skipping: the save sheet exposes no checkbox ({})",
            found["hint"]
        );
        cancel_and_close(&c).await;
        return;
    };
    let reff = boxes[0]["ref"].as_str().unwrap().to_string();
    let act = |action: &'static str| {
        let c = &c;
        let reff = reff.clone();
        async move {
            c.ok("ui_action", json!({ "ref": reff, "action": action }))
                .await
        }
    };

    // Whatever it was, it is now checked; asking again presses nothing.
    let first = act("check").await;
    assert_eq!(first["checked"], json!(true), "{first}");
    let second = act("check").await;
    assert_eq!(second["checked"], json!(true), "{second}");
    assert_eq!(second["changed"], json!(false), "{second}");

    let off = act("uncheck").await;
    assert_eq!(off["checked"], json!(false), "{off}");
    assert_eq!(off["changed"], json!(true), "{off}");
    let off_again = act("uncheck").await;
    assert_eq!(off_again["changed"], json!(false), "{off_again}");

    let flipped = act("toggle").await;
    assert_eq!(flipped["checked"], json!(true), "{flipped}");
    assert_eq!(flipped["changed"], json!(true), "{flipped}");

    // Put it back the way the sheet had it: unchecked when the first press
    // changed it, checked when it was already checked.
    if first["changed"] == json!(true) {
        act("uncheck").await;
    }
    cancel_and_close(&c).await;
}

/// `set_value` and `keyboard_type` say what the field shows afterwards, taken
/// from the OS and not from the request.
#[tokio::test(flavor = "multi_thread")]
async fn textedit_set_value_and_typing_report_what_the_document_shows() {
    if !ready() {
        return;
    }
    let c = client("live-readback");
    c.initialize().await;
    c.ok("launch", json!({ "app": APP })).await;
    if !settle_on_textedit(&c).await {
        return;
    }
    if !guarded_key(&c, "cmd+n").await {
        return;
    }
    tokio::time::sleep(Duration::from_millis(700)).await;

    // "text field" finds TextEdit's text area: role synonyms are part of the
    // contract, so this is also where they are exercised live.
    let found = c
        .ok(
            "find_elements",
            json!({ "app": APP, "role": "text field", "limit": 5 }),
        )
        .await;
    let Some(area) = found["elements"].as_array().and_then(|e| e.first()) else {
        eprintln!("skipping: no text area found ({})", found["hint"]);
        let _ = c.call("close_app", json!({ "app": APP })).await;
        return;
    };
    let reff = area["ref"].as_str().unwrap().to_string();

    let nonce = format!("readback-{}", mcp_policy::now_ms());
    let set = c
        .ok("set_value", json!({ "ref": &reff, "text": &nonce }))
        .await;
    assert_eq!(set["value_after"], json!(nonce), "{set}");
    assert_eq!(set["changed"], json!(true), "{set}");

    // Believe the tree, not the tool.
    let tree = c.ok("get_ui_tree", json!({ "app": APP })).await;
    assert!(
        tree["text"].as_str().unwrap_or_default().contains(&nonce),
        "set_value reported success but the document does not show the text"
    );

    // The same text again is not a change, and is still ok.
    let again = c
        .ok("set_value", json!({ "ref": &reff, "text": &nonce }))
        .await;
    assert_eq!(again["changed"], json!(false), "{again}");

    // Typing is read back too. Only while TextEdit is the app that will
    // receive the keystrokes.
    if !focused_is_textedit() {
        eprintln!("skipping typing: focus moved off {APP}");
        let _ = c.call("close_app", json!({ "app": APP })).await;
        return;
    }
    let typed = c
        .ok("keyboard_type", json!({ "ref": &reff, "text": "!" }))
        .await;
    assert_eq!(typed["changed"], json!(true), "{typed}");
    let shown = typed["value_after"].as_str().unwrap_or_default();
    assert!(
        shown.contains(&nonce) && shown.contains('!'),
        "the typed character must be in what the document shows: {typed}"
    );

    // A secret is written but never reported.
    let secret = c
        .ok(
            "set_value",
            json!({ "ref": &reff, "text": "s3cret-value", "secret": true }),
        )
        .await;
    assert!(secret.get("value_after").is_none(), "{secret}");

    let _ = c.call("close_app", json!({ "app": APP })).await;
}

/// The chord spellings a model reaches for are accepted, and a chord of two
/// keys is refused rather than half pressed.
#[tokio::test(flavor = "multi_thread")]
async fn shortcut_spellings_work_in_textedit_and_two_keys_are_refused() {
    if !ready() {
        return;
    }
    let c = client("live-chords");
    c.initialize().await;
    c.ok("launch", json!({ "app": APP })).await;
    if !settle_on_textedit(&c).await {
        return;
    }
    // A document to select in, via two spellings of the same chord.
    for chord in ["Cmd+N", "cmd-n"] {
        if !guarded_key(&c, chord).await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let env = c.call("keyboard_shortcut", json!({ "combo": "a+b" })).await;
    assert!(!env.ok, "two keys must be an error");
    assert_eq!(env.error.unwrap().code, ErrorCode::InvalidArgs);

    // Both documents were opened, so both spellings pressed the chord.
    let windows: Value = c.ok("list_windows", json!({ "app": APP })).await;
    let count = windows["windows"].as_array().map_or(0, Vec::len);
    assert!(
        count >= 2,
        "two new documents expected, saw {count}: {windows}"
    );
    let _ = c.call("close_app", json!({ "app": APP })).await;
}
