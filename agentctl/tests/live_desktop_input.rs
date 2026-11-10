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
    client_with(name, mcp_types::Foreground::Allow)
}

fn client_with(name: &str, foreground: mcp_types::Foreground) -> InProcClient {
    let mut cfg = test_policy(&[
        Category::Vision,
        Category::Input,
        Category::Window,
        Category::System,
    ]);
    cfg.allowed_apps = vec![APP.to_string(), "Finder".to_string()];
    cfg.foreground = foreground;
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

/// The document's text as the accessibility tree shows it, polled for a
/// moment because an app applies keystrokes on its own run loop.
async fn document_shows(c: &InProcClient, want: impl Fn(&str) -> bool) -> (bool, String) {
    let mut text = String::new();
    for _ in 0..20 {
        let tree = c.ok("get_ui_tree", json!({ "app": APP })).await;
        text = tree["text"].as_str().unwrap_or_default().to_string();
        if want(&text) {
            return (true, text);
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    (false, text)
}

/// A held Shift turns later arrow chords into a selection, and releasing it
/// ends that: the one after `up` moves the caret and selects nothing.
///
/// With Shift held, two Lefts select "bc" and typing replaces them ("aX");
/// after `up`, Left only moves the caret, so typing inserts ("aZX"). A Shift
/// that was never held, or never let go, gives a different document.
#[tokio::test(flavor = "multi_thread")]
async fn a_held_shift_selects_across_chords_and_up_lets_go() {
    if !ready() {
        return;
    }
    let c = client("live-hold");
    c.initialize().await;
    c.ok("launch", json!({ "app": APP })).await;
    if !settle_on_textedit(&c).await {
        return;
    }
    if !guarded_key(&c, "cmd+n").await {
        return;
    }
    tokio::time::sleep(Duration::from_millis(700)).await;
    if !focused_is_textedit() {
        eprintln!("skipping: focus moved off {APP}");
        return;
    }
    c.ok("keyboard_type", json!({ "text": "abc" })).await;

    let down = c
        .ok(
            "keyboard_shortcut",
            json!({ "combo": "shift", "action": "down" }),
        )
        .await;
    assert_eq!(down["held"], json!(["shift"]), "{down}");
    assert_eq!(down["auto_release_ms"], json!(30_000), "{down}");
    let mut ok = true;
    for _ in 0..2 {
        ok &= guarded_key(&c, "Left").await;
    }
    // Let go before anything else can fail, so a refused step cannot leave
    // Shift held.
    let up = c
        .ok(
            "keyboard_shortcut",
            json!({ "combo": "shift", "action": "up" }),
        )
        .await;
    assert_eq!(up["released"], json!(["shift"]), "{up}");
    assert_eq!(up["held"], json!([]), "{up}");
    if !ok || !focused_is_textedit() {
        eprintln!("skipping the rest: focus moved off {APP}");
        let _ = c.call("close_app", json!({ "app": APP })).await;
        return;
    }
    c.ok("keyboard_type", json!({ "text": "X" })).await;
    let (selected, text) = document_shows(&c, |t| t.contains("aX") && !t.contains("abX")).await;
    assert!(
        selected,
        "two shifted Lefts must select \"bc\": document shows {text:?}"
    );

    if guarded_key(&c, "Left").await {
        c.ok("keyboard_type", json!({ "text": "Z" })).await;
        let (released, text) = document_shows(&c, |t| t.contains("aZX")).await;
        assert!(
            released,
            "Shift must be up after `up`: document shows {text:?}"
        );
    }
    let _ = c.call("close_app", json!({ "app": APP })).await;
}

/// A drag with a `path` presses at the start, passes through the waypoints
/// and releases at the last: dragging along a line of text selects it, and
/// typing then replaces the selection.
#[tokio::test(flavor = "multi_thread")]
async fn a_drag_path_selects_a_line_of_text_and_reports_its_waypoints() {
    if !ready() {
        return;
    }
    let c = client("live-drag-path");
    c.initialize().await;
    c.ok("launch", json!({ "app": APP })).await;
    if !settle_on_textedit(&c).await {
        return;
    }
    if !guarded_key(&c, "cmd+n").await {
        return;
    }
    tokio::time::sleep(Duration::from_millis(700)).await;
    if !focused_is_textedit() {
        eprintln!("skipping: focus moved off {APP}");
        return;
    }
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
    c.ok("keyboard_type", json!({ "text": "wordwordword" }))
        .await;
    if !focused_is_textedit() {
        eprintln!("skipping: focus moved off {APP}");
        return;
    }

    // Offsets from the text area's top-left, along the first line and out
    // past its end.
    let drag = c
        .ok(
            "drag_drop",
            json!({
                "from": { "ref": &reff, "x": 3, "y": 10 },
                "path": [{ "x": 60, "y": 10 }, { "x": 200, "y": 10 }],
            }),
        )
        .await;
    assert_eq!(drag["waypoints"], json!(2), "{drag}");

    // A path outside the window is refused whole, before anything moves.
    let outside = c
        .call(
            "drag_drop",
            json!({ "from": { "ref": &reff, "x": 3, "y": 10 }, "path": [{ "x": 60, "y": 10 }, { "x": 99999, "y": 10 }] }),
        )
        .await;
    assert_eq!(outside.error.unwrap().code, ErrorCode::PolicyDenied);

    if !focused_is_textedit() {
        eprintln!("skipping typing: focus moved off {APP}");
        let _ = c.call("close_app", json!({ "app": APP })).await;
        return;
    }
    c.ok("keyboard_type", json!({ "text": "Q" })).await;
    let (replaced, text) = document_shows(&c, |t| t.contains('Q') && !t.contains("wordword")).await;
    assert!(
        replaced,
        "the dragged line must have been selected and replaced: document shows {text:?}"
    );
    let _ = c.call("close_app", json!({ "app": APP })).await;
}

/// A new TextEdit document holding `text`, and the ref of its text area.
/// `None` (after saying why) if TextEdit did not get there.
async fn scratch_document(c: &InProcClient, text: &str) -> Option<String> {
    c.ok("launch", json!({ "app": APP })).await;
    if !settle_on_textedit(c).await || !guarded_key(c, "cmd+n").await {
        return None;
    }
    tokio::time::sleep(Duration::from_millis(700)).await;
    let found = c
        .ok(
            "find_elements",
            json!({ "app": APP, "role": "text field", "limit": 5 }),
        )
        .await;
    let Some(area) = found["elements"].as_array().and_then(|e| e.first()) else {
        eprintln!("skipping: no text area found ({})", found["hint"]);
        let _ = c.call("close_app", json!({ "app": APP })).await;
        return None;
    };
    let reff = area["ref"].as_str().unwrap().to_string();
    c.ok("set_value", json!({ "ref": &reff, "text": text }))
        .await;
    Some(reff)
}

/// `select_text` sets the selection through the accessibility API, and what it
/// reports is what a separate read of the element says, in characters whatever
/// the platform counts in. The document holds a character outside the Basic
/// Multilingual Plane (two UTF-16 units) and a combining accent (its own
/// character) before most of the matches, so a unit mix-up lands on the wrong
/// text and fails here. The pointer must not move.
#[tokio::test(flavor = "multi_thread")]
async fn textedit_select_text_selects_by_text_and_offsets_without_the_pointer() {
    if !ready() {
        return;
    }
    use mcp_input::InputBackend;
    let c = client("live-select-text");
    c.initialize().await;
    // Characters: "alpha "=0..6 "beta"=6..10 " " 𝄞=11 " " "gamma"=13..18
    // " " e=19 U+0301=20 "nd"=21..23 " " "beta"=24..28 (28 in all).
    let text = "alpha beta \u{1D11E} gamma e\u{301}nd beta";
    let Some(reff) = scratch_document(&c, text).await else {
        return;
    };
    let backend = mcp_macos::MacosBackend::new();
    let pointer_before = backend.pointer_position().await.ok().flatten();

    let by_text = c
        .ok("select_text", json!({ "ref": &reff, "text": "gamma" }))
        .await;
    assert_eq!(by_text["input"], json!("ax"), "{by_text}");
    assert_eq!(
        (by_text["start"].as_u64(), by_text["end"].as_u64()),
        (Some(13), Some(18)),
        "{by_text}"
    );
    assert_eq!(by_text["text"], json!("gamma"), "{by_text}");

    // A separate call reads the element again: believe it, not the setter.
    let read = c.ok("get_selection", json!({ "ref": &reff })).await;
    assert_eq!(read["text"], json!("gamma"), "{read}");
    assert_eq!(
        (read["start"].as_u64(), read["end"].as_u64()),
        (Some(13), Some(18))
    );

    let second = c
        .ok(
            "select_text",
            json!({ "ref": &reff, "text": "beta", "occurrence": 2 }),
        )
        .await;
    assert_eq!(
        (second["start"].as_u64(), second["end"].as_u64()),
        (Some(24), Some(28)),
        "{second}"
    );

    let symbol = c
        .ok(
            "select_text",
            json!({ "ref": &reff, "start": 11, "end": 12 }),
        )
        .await;
    assert_eq!(symbol["text"], json!("\u{1D11E}"), "{symbol}");
    assert_eq!(symbol["length"], json!(1), "one character, not two units");

    let accent = c
        .ok("select_text", json!({ "ref": &reff, "text": "e\u{301}nd" }))
        .await;
    assert_eq!(
        (accent["start"].as_u64(), accent["end"].as_u64()),
        (Some(19), Some(23)),
        "{accent}"
    );

    let between = c
        .ok(
            "select_text",
            json!({ "ref": &reff, "from_text": "alpha", "to_text": "beta" }),
        )
        .await;
    assert_eq!(between["text"], json!("alpha beta"), "{between}");

    let all = c
        .ok("select_text", json!({ "ref": &reff, "all": true }))
        .await;
    assert_eq!(all["length"], json!(28), "{all}");
    assert_eq!(all["text"], json!(text), "{all}");

    let caret = c
        .ok("select_text", json!({ "ref": &reff, "start": 3, "end": 3 }))
        .await;
    assert_eq!(caret["length"], json!(0), "{caret}");

    // A range the text does not have is refused, and the selection stays.
    let past = c
        .call(
            "select_text",
            json!({ "ref": &reff, "start": 0, "end": 99 }),
        )
        .await;
    assert!(!past.ok);
    assert_eq!(past.error.unwrap().code, ErrorCode::InvalidArgs);
    let missing = c
        .call("select_text", json!({ "ref": &reff, "text": "mars" }))
        .await;
    assert!(!missing.ok);
    let still = c.ok("get_selection", json!({ "ref": &reff })).await;
    assert_eq!(still["length"], json!(0), "{still}");

    // The focused element, with no target, is the same text area.
    if focused_is_textedit() {
        c.ok("select_text", json!({ "ref": &reff, "text": "alpha" }))
            .await;
        let focused = c.ok("get_selection", json!({})).await;
        assert_eq!(focused["text"], json!("alpha"), "{focused}");
    }

    let pointer_after = backend.pointer_position().await.ok().flatten();
    assert_eq!(
        pointer_before, pointer_after,
        "select_text reported the accessibility route but the pointer moved"
    );
    let _ = c.call("close_app", json!({ "app": APP })).await;
}

/// `ui_action` only says ok for what it did. An action the element does not
/// offer is an error that lists the ones it does; one it does offer reports
/// the route. TextEdit's text area has no AXIncrement.
#[tokio::test(flavor = "multi_thread")]
async fn textedit_ui_action_names_the_actions_an_element_lacks() {
    if !ready() {
        return;
    }
    let c = client("live-ax-ladder");
    c.initialize().await;
    let Some(reff) = scratch_document(&c, "ladder").await else {
        return;
    };

    let step = c
        .call("ui_action", json!({ "ref": &reff, "action": "increment" }))
        .await;
    assert!(!step.ok, "a text area cannot be incremented: {step:?}");
    let err = step.error.unwrap();
    assert_eq!(err.code, ErrorCode::UnsupportedOs, "{}", err.message);
    assert!(
        err.message.contains("does not offer AXIncrement") && err.message.contains("it offers"),
        "{}",
        err.message
    );

    // Whether the text area offers a context menu is the app's to say; either
    // answer must be honest: an ok carries the route, a refusal names actions.
    let menu = c
        .call("ui_action", json!({ "ref": &reff, "action": "show_menu" }))
        .await;
    if menu.ok {
        assert_eq!(menu.data.unwrap()["input"], json!("ax"));
        // The menu is open and would swallow the next call.
        if focused_is_textedit() {
            let _ = c
                .call("keyboard_shortcut", json!({ "combo": "Escape" }))
                .await;
        }
    } else {
        assert!(menu.error.unwrap().message.contains("it offers"));
    }

    let focus = c
        .ok("ui_action", json!({ "ref": &reff, "action": "focus" }))
        .await;
    assert_eq!(focus["input"], json!("ax"), "{focus}");
    let _ = c.call("close_app", json!({ "app": APP })).await;
}

/// The app that owns the frontmost window right now, read by a backend that
/// has pinned nothing.
fn frontmost() -> Option<String> {
    use mcp_input::InputBackend;
    mcp_macos::MacosBackend::new().input_target()
}

/// With `foreground = "never"`, the accessibility tools act on a TextEdit
/// document that is in the background, and the user's frontmost app and
/// pointer stay where they were; the pointer and keyboard tools are refused.
///
/// The document is set up under `allow` (that needs TextEdit forward), then
/// Finder is brought forward and a second server, configured `never`, does the
/// work. Nothing in the second half may change who is frontmost, so the test
/// reads it again after every step, not once at the end.
#[tokio::test(flavor = "multi_thread")]
async fn under_never_the_accessibility_tools_work_without_taking_the_foreground() {
    if !ready() {
        return;
    }
    use mcp_input::InputBackend;
    let setup = client("live-never-setup");
    setup.initialize().await;
    let text = "alpha beta gamma";
    if scratch_document(&setup, text).await.is_none() {
        return;
    }
    // Put something else in front: this is the user's app.
    let _ = setup.call("focus_app", json!({ "app": "Finder" })).await;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !frontmost().is_some_and(|f| f.contains("Finder")) {
        if Instant::now() > deadline {
            eprintln!("skipping: Finder never became frontmost; cannot show TextEdit stays behind");
            let _ = setup.call("close_app", json!({ "app": APP })).await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    let before = frontmost();
    let backend = mcp_macos::MacosBackend::new();
    let pointer_before = backend.pointer_position().await.ok().flatten();

    let c = client_with("live-never", mcp_types::Foreground::Never);
    c.initialize().await;
    let still_behind = |step: &str| {
        assert_eq!(
            frontmost(),
            before,
            "{step} changed which app is frontmost under foreground = never"
        );
    };

    // `app` targets TextEdit for the lookup without raising it.
    let found = c
        .ok(
            "find_elements",
            json!({ "app": APP, "role": "text field", "limit": 5 }),
        )
        .await;
    let reff = found["elements"][0]["ref"]
        .as_str()
        .expect("TextEdit's text area")
        .to_string();
    still_behind("find_elements");

    let set = c
        .ok(
            "set_value",
            json!({ "ref": &reff, "text": "alpha beta gamma delta" }),
        )
        .await;
    assert_eq!(set["input"], json!("ax"), "{set}");
    still_behind("set_value");

    let selected = c
        .ok("select_text", json!({ "ref": &reff, "text": "gamma" }))
        .await;
    assert_eq!(selected["input"], json!("ax"), "{selected}");
    assert_eq!(selected["text"], json!("gamma"), "{selected}");
    still_behind("select_text");

    let acted = c
        .ok(
            "ui_action",
            json!({ "ref": &reff, "action": "scroll_into_view" }),
        )
        .await;
    assert_eq!(acted["input"], json!("ax"), "{acted}");
    still_behind("ui_action");

    // What the document shows is what was written, read back separately.
    let shown = c.ok("get_selection", json!({ "ref": &reff })).await;
    assert_eq!(shown["text"], json!("gamma"), "{shown}");

    // Everything that would have used the pointer, the keyboard or the front
    // is refused, and refused before it does anything.
    for (tool, args) in [
        (
            "mouse_action",
            json!({ "type": "click", "x": 100, "y": 100 }),
        ),
        ("mouse_action", json!({ "type": "click", "ref": &reff })),
        ("keyboard_type", json!({ "text": "x", "ref": &reff })),
        ("keyboard_shortcut", json!({ "combo": "cmd+a" })),
        ("ui_action", json!({ "ref": &reff, "action": "focus" })),
        ("focus_app", json!({ "app": APP })),
        ("control_window", json!({ "action": "focus" })),
    ] {
        let env = c.call(tool, args.clone()).await;
        assert!(!env.ok, "{tool} {args} ran under never");
        assert_eq!(
            env.error.expect("refusal").code,
            ErrorCode::PolicyDenied,
            "{tool} {args}"
        );
        still_behind(tool);
    }

    let pointer_after = backend.pointer_position().await.ok().flatten();
    assert_eq!(
        pointer_before, pointer_after,
        "the pointer moved under foreground = never"
    );
    let _ = setup.call("close_app", json!({ "app": APP })).await;
}

/// Several actions in one call, against a real document: the steps run in
/// order through the same gate as single calls, each reports what it did, and
/// the document shows the result. A step that fails ends the batch and the
/// ones after it leave the document alone.
#[tokio::test(flavor = "multi_thread")]
async fn textedit_input_steps_edit_a_scratch_document_in_one_call() {
    if !ready() {
        return;
    }
    let c = client("live-input-steps");
    c.initialize().await;
    let Some(reff) = scratch_document(&c, "").await else {
        return;
    };
    // The keystroke step goes to whatever is frontmost, so check it is TextEdit
    // now, as every guarded chord in this file does.
    if !focused_is_textedit() {
        eprintln!("skipping: focus moved off {APP}");
        let _ = c.call("close_app", json!({ "app": APP })).await;
        return;
    }
    let env = c
        .call(
            "input_steps",
            json!({ "steps": [
                { "tool": "set_value", "args": { "ref": &reff, "text": "hello batch" } },
                { "tool": "select_text", "args": { "ref": &reff, "text": "batch" } },
                { "tool": "keyboard_type", "args": { "text": "world", "effects": false } }
            ]}),
        )
        .await;
    assert!(env.ok, "{env:?}");
    let data = env.data.unwrap();
    assert_eq!(data["ran"], json!(3), "{data}");
    let tools: Vec<_> = data["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["tool"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(tools, ["set_value", "select_text", "keyboard_type"]);
    let (done, text) = document_shows(&c, |t| t == "hello world").await;
    assert!(
        done,
        "the batch must have left the document as hello world: {text:?}"
    );

    // The second step cannot find its element: the batch stops there, says
    // which step it was, and the third step never types.
    let env = c
        .call(
            "input_steps",
            json!({ "steps": [
                { "tool": "set_value", "args": { "ref": &reff, "text": "first" } },
                { "tool": "ui_action", "args": { "ref": "@e99999", "action": "click" } },
                { "tool": "set_value", "args": { "ref": &reff, "text": "never" } }
            ]}),
        )
        .await;
    assert!(!env.ok);
    let data = env.data.unwrap();
    assert_eq!(data["failed_at"], json!(1), "{data}");
    let (stopped, text) = document_shows(&c, |t| t == "first").await;
    assert!(stopped, "step 2 must not have run: {text:?}");
    let _ = c.call("close_app", json!({ "app": APP })).await;
}

/// `effects` has to say what the app really did. Cmd+S on a new document
/// slides a Save sheet off the window; Escape takes it away again. The
/// answer comes with the call that caused it, with no second look at the tree.
#[tokio::test(flavor = "multi_thread")]
async fn effects_report_the_save_sheet_a_shortcut_opens_and_closes() {
    if !ready() {
        return;
    }
    let c = client("live-effects-sheet");
    c.initialize().await;
    c.ok("launch", json!({ "app": APP })).await;
    if !settle_on_textedit(&c).await || !guarded_key(&c, "cmd+n").await {
        return;
    }
    tokio::time::sleep(Duration::from_millis(700)).await;
    // Look once so the session is aimed at TextEdit before the action.
    c.ok("get_ui_tree", json!({ "app": APP })).await;
    if !focused_is_textedit() {
        return;
    }
    let opened = c.ok("keyboard_shortcut", json!({ "combo": "cmd+s" })).await;
    let sheet = &opened["effects"]["overlay"];
    if sheet["kind"] != json!("sheet") {
        eprintln!("skipping: no sheet was reported (the sheet may take longer than the settle wait here): {opened}");
        cancel_and_close(&c).await;
        return;
    }
    // The question and its choices come with it: nothing else to ask for.
    assert!(
        sheet["buttons"]
            .as_array()
            .is_some_and(|b| b.iter().any(|t| t == "Cancel")),
        "{sheet}"
    );
    if !focused_is_textedit() {
        return;
    }
    let closed = c
        .ok("keyboard_shortcut", json!({ "combo": "Escape" }))
        .await;
    assert_eq!(
        closed["effects"]["overlay_closed"],
        json!("sheet"),
        "{closed}"
    );
    // Nothing changed, nothing said: moving the caret in a plain document
    // carries no effects.
    let quiet = c.ok("keyboard_shortcut", json!({ "combo": "Right" })).await;
    assert!(quiet.get("effects").is_none(), "{quiet}");
    let _ = c.call("close_app", json!({ "app": APP })).await;
}
