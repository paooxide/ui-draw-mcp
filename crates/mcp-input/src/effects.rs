//! `effects`: what an input action changed, without a snapshot.
//!
//! An agent that clicks "Save" has to know whether a sheet opened, a window
//! closed or focus moved before it can choose the next call, and the only way
//! the desktop tools offered to find out was another `get_ui_tree`. That is a
//! whole model turn per action. The browser engine already answers in the same
//! call (`browser_act` `effects`); this is the desktop twin.
//!
//! It is deliberately not a snapshot diff. A full tree walk before and after
//! every action would cost more than the turn it saves. A [`UiProbe`] reads a
//! small fixed set of facts (the frontmost app, the focused element, the target
//! app's window titles, the sheets and dialogs on it), and [`diff_facts`], a
//! pure function, compares two readings.
//!
//! Three rules keep the result honest and small:
//!
//! * A fact the platform could not read is `None`, and a fact missing from
//!   either reading is never compared. Nothing is guessed.
//! * Only what changed is emitted, and nothing at all when nothing did. So an
//!   absent `effects` means "nothing observed to change", which is not proof
//!   that nothing changed: the facts are few, and an app may react after the
//!   settle wait.
//! * Every string is clipped and every list is capped.

use async_trait::async_trait;
use serde_json::{json, Map, Value};

use crate::backend::Reading;

/// How long to let an app react before reading again. The browser engine waits
/// 120 ms for the same reason; a desktop app answers a synthetic click or key a
/// beat later, and a sheet is added to the window on the app's own run loop.
pub const SETTLE_MS: u64 = 120;

/// Most items of a list reported, then `+N more`. The browser's number.
const ITEMS: usize = 5;
/// Characters of a window title, a name or a message.
const NAME_CHARS: usize = 60;
/// Characters of a changed value.
const VALUE_CHARS: usize = 80;
/// Characters of the question a dialog asks.
const TEXT_CHARS: usize = 100;
/// Buttons listed on a sheet or dialog.
const BUTTONS: usize = 6;
/// Characters of a button title.
const BUTTON_CHARS: usize = 24;

/// The element that has keyboard focus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Focus {
    /// Normalised role (`textfield`, `button`), as `get_ui_tree` spells it.
    pub role: String,
    /// What it is called, when it has a label. Never its value.
    pub name: Option<String>,
}

/// A sheet, dialog, alert or open menu in the way of the app's windows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overlay {
    /// `sheet`, `dialog`, `alert`, `popover` or `menu`.
    pub kind: String,
    pub title: Option<String>,
    /// The question it asks, when it was read.
    pub text: Option<String>,
    /// The choices it offers, when they were read.
    pub buttons: Vec<String>,
}

impl Overlay {
    /// The same overlay, whether or not its details were read: details are
    /// read only for new ones, so they cannot take part in the comparison.
    fn same_as(&self, other: &Overlay) -> bool {
        self.kind == other.kind && self.title == other.title
    }
}

/// The small fixed set of facts read before and after an action.
///
/// Each is `None` when it could not be read, which is not the same as "there
/// is nothing": an empty window list is `Some(vec![])`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UiFacts {
    /// The application in front.
    pub frontmost: Option<String>,
    pub focus: Option<Focus>,
    /// Titles of the target app's windows, `""` for one without a title.
    pub windows: Option<Vec<String>>,
    pub overlays: Option<Vec<Overlay>>,
}

/// Reads [`UiFacts`] from the OS.
///
/// An implementation must be passive: it may not raise a window, move focus,
/// post an event or change the sticky target, so reading is as safe under
/// `foreground = "never"` as it is without. It must also be quick, since it
/// runs twice around every action.
#[async_trait]
pub trait UiProbe: Send + Sync {
    /// Read the facts about the application input is aimed at.
    ///
    /// `describe_new` is `None` for a first reading, which names overlays by
    /// kind and title only. With `Some(known)`, an overlay that is not among
    /// `known` is also read for the question it asks and its buttons, so the
    /// price of reading a dialog's contents is paid once, when it appears.
    async fn probe(&self, describe_new: Option<&[Overlay]>) -> UiFacts;
}

/// One line, at most `max` characters, with an ellipsis when cut.
pub fn clip(s: &str, max: usize) -> String {
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= max {
        return one;
    }
    let mut out: String = one.chars().take(max.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

fn shown(title: &str) -> String {
    if title.trim().is_empty() {
        "(untitled)".into()
    } else {
        clip(title, NAME_CHARS)
    }
}

/// Items in `after` that `before` does not have as many of, and the reverse.
/// A multiset difference: two windows called "Untitled" are two windows.
fn multiset_diff(before: &[String], after: &[String]) -> (Vec<String>, Vec<String>) {
    let mut left: Vec<&String> = before.iter().collect();
    let mut added = Vec::new();
    for a in after {
        match left.iter().position(|b| *b == a) {
            Some(i) => {
                left.swap_remove(i);
            }
            None => added.push(a.clone()),
        }
    }
    (added, left.into_iter().cloned().collect())
}

fn listed(items: &[String]) -> Value {
    let mut out: Vec<String> = items.iter().take(ITEMS).map(|t| shown(t)).collect();
    if items.len() > out.len() {
        out.push(format!("+{} more", items.len() - out.len()));
    }
    json!(out)
}

fn overlay_json(o: &Overlay) -> Value {
    let mut out = json!({ "kind": o.kind });
    if let Some(t) = o.title.as_deref().filter(|t| !t.trim().is_empty()) {
        out["title"] = json!(clip(t, NAME_CHARS));
    }
    if let Some(t) = o.text.as_deref().filter(|t| !t.trim().is_empty()) {
        out["text"] = json!(clip(t, TEXT_CHARS));
    }
    if !o.buttons.is_empty() {
        let mut b: Vec<String> = o
            .buttons
            .iter()
            .take(BUTTONS)
            .map(|b| clip(b, BUTTON_CHARS))
            .collect();
        if o.buttons.len() > b.len() {
            b.push(format!("+{} more", o.buttons.len() - b.len()));
        }
        out["buttons"] = json!(b);
    }
    out
}

/// What the target element showed, for the tools that do not read it back
/// themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElementFacts {
    pub reading: Reading,
    /// A password field's value is never reported, changed or not.
    pub secure: bool,
}

/// Compare two readings and describe only what changed, or `None` if nothing
/// did. Field names follow `browser_act` where the idea is the same: `focus`,
/// `appeared`, `disappeared`, `overlay`.
pub fn diff_facts(
    before: &UiFacts,
    after: &UiFacts,
    element: Option<(&ElementFacts, &ElementFacts)>,
) -> Option<Value> {
    let mut fx = Map::new();

    if let (Some(b), Some(a)) = (&before.frontmost, &after.frontmost) {
        if b != a {
            fx.insert("app".into(), json!(clip(a, NAME_CHARS)));
        }
    }

    if let (Some(b), Some(a)) = (&before.focus, &after.focus) {
        if b != a {
            let mut f = json!({ "role": a.role });
            if let Some(n) = a.name.as_deref().filter(|n| !n.trim().is_empty()) {
                f["name"] = json!(clip(n, NAME_CHARS));
            }
            fx.insert("focus".into(), f);
        }
    }

    if let (Some(b), Some(a)) = (&before.windows, &after.windows) {
        let (opened, closed) = multiset_diff(b, a);
        // One window gone and one new, in an app that kept its count, is a
        // window that was renamed (Untitled saved as notes.txt) far more often
        // than a coincidence, and it reads shorter.
        if let ([new], [old]) = (opened.as_slice(), closed.as_slice()) {
            fx.insert(
                "retitled".into(),
                json!({ "from": shown(old), "to": shown(new) }),
            );
        } else {
            if !opened.is_empty() {
                fx.insert("appeared".into(), listed(&opened));
            }
            if !closed.is_empty() {
                fx.insert("disappeared".into(), listed(&closed));
            }
        }
    }

    if let (Some(b), Some(a)) = (&before.overlays, &after.overlays) {
        if let Some(new) = a.iter().find(|o| !b.iter().any(|k| k.same_as(o))) {
            fx.insert("overlay".into(), overlay_json(new));
        }
        if let Some(gone) = b.iter().find(|o| !a.iter().any(|k| k.same_as(o))) {
            fx.insert("overlay_closed".into(), json!(gone.kind));
        }
    }

    if let Some((b, a)) = element {
        if !a.secure && !b.secure {
            if let (Some(bv), Some(av)) = (&b.reading.value, &a.reading.value) {
                if bv != av {
                    fx.insert("value".into(), json!(clip(av, VALUE_CHARS)));
                }
            }
            if let (Some(bc), Some(ac)) = (b.reading.checked, a.reading.checked) {
                if bc != ac {
                    fx.insert("checked".into(), json!(ac));
                }
            }
        }
    }

    (!fx.is_empty()).then_some(Value::Object(fx))
}

/// Whether a call wants its effects reported. `effects: false` skips the two
/// reads and the settle wait, for a caller that is about to look anyway or is
/// sending a long run of keystrokes. Anything but a boolean is an error, so a
/// typo is not mistaken for a choice.
pub fn wanted(args: &Value) -> Result<bool, String> {
    match args.get("effects") {
        None | Some(Value::Null) => Ok(true),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err("'effects' is true or false".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(windows: &[&str]) -> UiFacts {
        UiFacts {
            frontmost: Some("TextEdit".into()),
            focus: Some(Focus {
                role: "textarea".into(),
                name: None,
            }),
            windows: Some(windows.iter().map(|s| s.to_string()).collect()),
            overlays: Some(Vec::new()),
        }
    }

    fn sheet(title: &str) -> Overlay {
        Overlay {
            kind: "sheet".into(),
            title: Some(title.into()),
            text: None,
            buttons: Vec::new(),
        }
    }

    #[test]
    fn nothing_changed_is_no_effects_at_all() {
        let f = facts(&["Untitled"]);
        assert_eq!(diff_facts(&f, &f.clone(), None), None);
    }

    #[test]
    fn a_new_sheet_is_reported_with_its_question_and_buttons() {
        let before = facts(&["Untitled"]);
        let mut after = before.clone();
        after.overlays = Some(vec![Overlay {
            kind: "sheet".into(),
            title: Some("Save".into()),
            text: Some("Do you want to keep this document?".into()),
            buttons: vec!["Cancel".into(), "Save".into()],
        }]);
        let fx = diff_facts(&before, &after, None).unwrap();
        assert_eq!(fx["overlay"]["kind"], "sheet");
        assert_eq!(fx["overlay"]["buttons"], json!(["Cancel", "Save"]));
        assert_eq!(fx["overlay"]["text"], "Do you want to keep this document?");
        assert_eq!(fx.as_object().unwrap().len(), 1, "{fx}");
    }

    #[test]
    fn an_overlay_that_was_already_there_is_not_new_even_if_only_one_side_read_its_details() {
        let mut before = facts(&["A"]);
        before.overlays = Some(vec![sheet("Save")]);
        let mut after = before.clone();
        after.overlays = Some(vec![Overlay {
            buttons: vec!["Save".into()],
            ..sheet("Save")
        }]);
        assert_eq!(diff_facts(&before, &after, None), None);
    }

    #[test]
    fn a_closed_sheet_says_what_kind() {
        let mut before = facts(&["A"]);
        before.overlays = Some(vec![sheet("Save")]);
        let after = facts(&["A"]);
        let fx = diff_facts(&before, &after, None).unwrap();
        assert_eq!(fx["overlay_closed"], "sheet");
        assert!(fx.get("overlay").is_none());
    }

    #[test]
    fn windows_are_compared_as_a_multiset() {
        let (opened, closed) = multiset_diff(
            &["Untitled".into(), "Untitled".into()],
            &["Untitled".into()],
        );
        assert!(opened.is_empty());
        assert_eq!(closed, vec!["Untitled".to_string()]);
        let (opened, closed) = multiset_diff(&["a".into()], &["a".into(), "b".into(), "b".into()]);
        assert_eq!(opened, vec!["b".to_string(), "b".to_string()]);
        assert!(closed.is_empty());
    }

    #[test]
    fn one_window_swapped_for_another_is_a_retitle() {
        let fx = diff_facts(&facts(&["Untitled"]), &facts(&["notes.txt"]), None).unwrap();
        assert_eq!(
            fx["retitled"],
            json!({ "from": "Untitled", "to": "notes.txt" })
        );
        assert!(fx.get("appeared").is_none() && fx.get("disappeared").is_none());
    }

    #[test]
    fn windows_that_open_and_close_are_listed_and_capped() {
        let before = facts(&["a"]);
        let many: Vec<String> = (0..9).map(|i| format!("w{i}")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let fx = diff_facts(&before, &facts(&refs), None).unwrap();
        let appeared = fx["appeared"].as_array().unwrap();
        assert_eq!(appeared.len(), ITEMS + 1);
        assert_eq!(appeared[ITEMS], "+4 more");
        assert_eq!(fx["disappeared"], json!(["a"]));
    }

    #[test]
    fn an_untitled_window_is_named_as_such() {
        let fx = diff_facts(&facts(&["a"]), &facts(&["a", ""]), None).unwrap();
        assert_eq!(fx["appeared"], json!(["(untitled)"]));
    }

    #[test]
    fn focus_and_the_frontmost_app_are_reported_when_they_move() {
        let before = facts(&["a"]);
        let mut after = before.clone();
        after.frontmost = Some("Finder".into());
        after.focus = Some(Focus {
            role: "button".into(),
            name: Some("Save".into()),
        });
        let fx = diff_facts(&before, &after, None).unwrap();
        assert_eq!(fx["app"], "Finder");
        assert_eq!(fx["focus"], json!({ "role": "button", "name": "Save" }));
    }

    /// The honesty rule: a fact one side could not read is not compared, so a
    /// failed read never turns into "everything changed".
    #[test]
    fn a_fact_that_could_not_be_read_is_never_reported() {
        let before = facts(&["a", "b"]);
        let blind = UiFacts::default();
        assert_eq!(diff_facts(&before, &blind, None), None);
        assert_eq!(diff_facts(&blind, &before, None), None);
        let mut half = before.clone();
        half.windows = None;
        half.overlays = None;
        assert_eq!(diff_facts(&before, &half, None), None);
    }

    #[test]
    fn a_changed_value_or_checkbox_is_reported_and_a_password_never_is() {
        let el = |v: &str, c: Option<bool>, secure| ElementFacts {
            reading: Reading {
                value: Some(v.into()),
                checked: c,
            },
            secure,
        };
        let f = facts(&["a"]);
        let fx = diff_facts(
            &f,
            &f,
            Some((&el("1", Some(false), false), &el("2", Some(true), false))),
        )
        .unwrap();
        assert_eq!(fx["value"], "2");
        assert_eq!(fx["checked"], true);
        let same = diff_facts(&f, &f, Some((&el("1", None, false), &el("1", None, false))));
        assert_eq!(same, None);
        let secret = diff_facts(&f, &f, Some((&el("a", None, true), &el("b", None, true))));
        assert_eq!(secret, None, "a password field's value is not reported");
    }

    #[test]
    fn text_is_clipped_to_one_short_line() {
        assert_eq!(clip("a\n  b\tc", 10), "a b c");
        let long = "x".repeat(200);
        let c = clip(&long, 20);
        assert_eq!(c.chars().count(), 20);
        assert!(c.ends_with('\u{2026}'));
        let mut after = facts(&["a"]);
        after.overlays = Some(vec![Overlay {
            kind: "dialog".into(),
            title: Some(long.clone()),
            text: Some(long.clone()),
            buttons: (0..9).map(|i| format!("button {i} {long}")).collect(),
        }]);
        let fx = diff_facts(&facts(&["a"]), &after, None).unwrap();
        let o = &fx["overlay"];
        assert_eq!(o["title"].as_str().unwrap().chars().count(), NAME_CHARS);
        assert_eq!(o["text"].as_str().unwrap().chars().count(), TEXT_CHARS);
        let b = o["buttons"].as_array().unwrap();
        assert_eq!(b.len(), BUTTONS + 1);
        assert_eq!(b[BUTTONS], "+3 more");
        assert!(b[0].as_str().unwrap().chars().count() <= BUTTON_CHARS);
    }

    #[test]
    fn effects_are_on_unless_switched_off() {
        assert_eq!(wanted(&json!({})), Ok(true));
        assert_eq!(wanted(&json!({"effects": null})), Ok(true));
        assert_eq!(wanted(&json!({"effects": true})), Ok(true));
        assert_eq!(wanted(&json!({"effects": false})), Ok(false));
        assert!(wanted(&json!({"effects": "soon"})).is_err());
        assert!(wanted(&json!({"effects": 0})).is_err());
    }
}
