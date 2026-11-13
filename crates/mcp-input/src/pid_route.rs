//! Judging whether keys sent to a background app arrived.
//!
//! Per-process key events are fire and forget: the call returns the same
//! whether the app took the keys or dropped them, and some apps (Electron
//! shells, AppKit text views of an inactive app) do drop them. So the only
//! honest "it worked" is a change in the app's own focused element. These are
//! the pure rules for reading that change; the reads are the backend's.

use crate::FocusedText;

/// What a before-and-after pair of reads says about typed text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// The element's text or caret moved: the keys arrived.
    Landed,
    /// Both reads worked and nothing changed: the keys did not arrive (or the
    /// field refused them).
    NotLanded,
    /// The element cannot be read, or is a password field: whether the keys
    /// arrived is unknown, and is never reported as success.
    Unverifiable,
}

/// Whether anything about the element changed. `None` when the reads cannot
/// say, which is not the same as "no".
pub fn changed(before: &FocusedText, after: &FocusedText) -> Option<bool> {
    if before.secure || after.secure {
        return None;
    }
    // Compare what both sides could read. A value that is readable only after
    // is a change from nothing, but not one that can be trusted as typed text.
    match (&before.value, &after.value) {
        (Some(a), Some(b)) if a != b => return Some(true),
        (Some(_), Some(_)) => {}
        _ => {
            return match (before.selection, after.selection) {
                (Some(a), Some(b)) => Some(a != b),
                _ => None,
            }
        }
    }
    Some(match (before.selection, after.selection) {
        (Some(a), Some(b)) => a != b,
        _ => false,
    })
}

/// Did `typed` arrive, judged from the focused element before and after.
pub fn judge_typed(before: &FocusedText, after: &FocusedText, typed: &str) -> Delivery {
    if typed.is_empty() {
        return Delivery::Landed;
    }
    // A caret move alone is weak evidence for text: arrow-like movement could
    // have come from anywhere. Require the value itself to be readable.
    if before.value.is_none() || after.value.is_none() {
        return Delivery::Unverifiable;
    }
    match changed(before, after) {
        Some(true) => Delivery::Landed,
        Some(false) => Delivery::NotLanded,
        None => Delivery::Unverifiable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(value: Option<&str>, sel: Option<(usize, usize)>) -> FocusedText {
        FocusedText {
            value: value.map(str::to_string),
            selection: sel,
            secure: false,
        }
    }

    #[test]
    fn typed_text_that_changed_the_value_landed() {
        let b = at(Some("ab"), Some((2, 2)));
        let a = at(Some("abxy"), Some((4, 4)));
        assert_eq!(judge_typed(&b, &a, "xy"), Delivery::Landed);
    }

    #[test]
    fn typing_that_changed_nothing_did_not_land() {
        let b = at(Some("ab"), Some((2, 2)));
        assert_eq!(judge_typed(&b, &b.clone(), "xy"), Delivery::NotLanded);
    }

    #[test]
    fn a_replaced_selection_with_the_same_text_still_counts_by_caret() {
        // Typing "b" over a selected "b" leaves the value as it was and
        // collapses the selection: a change the caret shows.
        let b = at(Some("abc"), Some((1, 2)));
        let a = at(Some("abc"), Some((2, 2)));
        assert_eq!(judge_typed(&b, &a, "b"), Delivery::Landed);
    }

    #[test]
    fn an_unreadable_element_is_unverifiable_never_landed() {
        let none = at(None, None);
        assert_eq!(judge_typed(&none, &none, "x"), Delivery::Unverifiable);
        // A caret alone does not vouch for text.
        let b = at(None, Some((0, 0)));
        let a = at(None, Some((1, 1)));
        assert_eq!(judge_typed(&b, &a, "x"), Delivery::Unverifiable);
    }

    #[test]
    fn a_password_field_is_unverifiable() {
        let p = FocusedText {
            secure: true,
            ..Default::default()
        };
        assert_eq!(judge_typed(&p, &p.clone(), "x"), Delivery::Unverifiable);
        assert_eq!(changed(&p, &p.clone()), None);
    }

    #[test]
    fn nothing_to_type_is_trivially_delivered() {
        let n = at(None, None);
        assert_eq!(judge_typed(&n, &n.clone(), ""), Delivery::Landed);
    }

    #[test]
    fn a_shortcut_is_verified_only_by_a_visible_change() {
        let b = at(Some("abc"), Some((0, 0)));
        let a = at(Some("abc"), Some((0, 3)));
        assert_eq!(changed(&b, &a), Some(true));
        assert_eq!(changed(&b, &b.clone()), Some(false));
        assert_eq!(changed(&at(None, None), &a), None);
    }
}
