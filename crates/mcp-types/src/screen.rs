//! Whether the desktop session can receive synthetic input at all, and the
//! refusal that says so.
//!
//! While the screen is locked, or the session has been switched away from the
//! console, the system discards key and pointer events a program posts, and
//! the post itself still succeeds: nothing in the posting call says the event
//! went nowhere. An action that reports "done" for that is a false report, so
//! the check is made before posting and the refusal names the cause. Lives in
//! the dependency root for the same reason `foreground` does: the input
//! engine, the window engine and the desktop engine all have to say the same
//! thing.

use crate::{Envelope, ErrorCode};

/// What the platform says about the session's ability to take input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenState {
    /// The session is on the console and unlocked: events are delivered.
    Ready,
    /// The lock screen is up. Events a program posts are discarded.
    Locked,
    /// The session is not the one on the console (another user is logged in
    /// at the keyboard). Its events are discarded.
    OffConsole,
    /// The platform cannot say, or this backend has no way to ask. Never a
    /// reason to refuse: an unknown is reported as unknown, not as locked.
    Unknown,
}

impl ScreenState {
    /// The spelling `agentctl doctor` and `idle_status` report.
    pub fn label(self) -> &'static str {
        match self {
            ScreenState::Ready => "ok",
            ScreenState::Locked => "locked",
            ScreenState::OffConsole => "off_console",
            ScreenState::Unknown => "unknown",
        }
    }

    /// The state from whatever lock indicators a platform could read, each
    /// `Some(locked)` or `None` when that source did not answer. One source
    /// saying locked is enough; with none answering it is unknown, which is
    /// not the same as unlocked.
    pub fn from_lock_hints(hints: &[Option<bool>]) -> ScreenState {
        if hints.contains(&Some(true)) {
            ScreenState::Locked
        } else if hints.contains(&Some(false)) {
            ScreenState::Ready
        } else {
            ScreenState::Unknown
        }
    }

    /// Whether synthetic input would be discarded.
    pub fn blocks_input(self) -> bool {
        matches!(self, ScreenState::Locked | ScreenState::OffConsole)
    }
}

/// A call refused because the session cannot receive synthetic input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenRefusal {
    pub state: ScreenState,
    /// What was refused, as a noun phrase ("mouse_action click").
    pub what: String,
}

/// The decision: `None` when `what` may go ahead, the refusal when the
/// session would throw its input away. Pure, so the wording and the choice of
/// states are tested without a screen.
pub fn screen_refusal(state: ScreenState, what: &str) -> Option<ScreenRefusal> {
    state.blocks_input().then(|| ScreenRefusal {
        state,
        what: what.to_string(),
    })
}

impl ScreenRefusal {
    fn cause(&self) -> &'static str {
        match self.state {
            ScreenState::OffConsole => "another user's session is on the console, not this one",
            _ => "the screen is locked",
        }
    }

    /// Says what was refused, why, and that nothing happened.
    pub fn message(&self) -> String {
        format!(
            "{} was refused and nothing was done: {}, and the system discards key and pointer \
             events a program posts to a session in that state (the post would look like it \
             worked and nothing would arrive)",
            self.what,
            self.cause()
        )
    }

    /// The way forward. Retryable, and honest about what a locked screen
    /// leaves working.
    pub fn suggestion(&self) -> String {
        let back = match self.state {
            ScreenState::OffConsole => "switch back to this user's session",
            _ => "unlock the screen",
        };
        format!(
            "retryable: {back} (or ask the person to) and call again. Meanwhile list_windows \
             and menu_list still answer, but get_ui_tree comes back with little more than the \
             application and screen or window capture fails, so there is nothing reliable to \
             read or act on"
        )
    }

    /// The tool result. `ACTION_FAILED`: there is no dedicated "precondition"
    /// code, and this is neither a permission (nothing to grant) nor a policy
    /// (nobody configured it); the suggestion carries "retryable".
    pub fn envelope(&self, tool: &str) -> Envelope {
        Envelope::fail_with(
            tool,
            ErrorCode::ActionFailed,
            self.message(),
            self.suggestion(),
        )
    }

    /// Message and suggestion in one string, for the backend error types that
    /// carry a single string.
    pub fn one_line(&self) -> String {
        format!("{}. {}", self.message(), self.suggestion())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_locked_or_switched_away_session_refuses() {
        assert!(screen_refusal(ScreenState::Ready, "keyboard_type").is_none());
        // Not knowing is never a reason to refuse: an unknown is not a lock.
        assert!(screen_refusal(ScreenState::Unknown, "keyboard_type").is_none());
        assert!(screen_refusal(ScreenState::Locked, "keyboard_type").is_some());
        assert!(screen_refusal(ScreenState::OffConsole, "keyboard_type").is_some());
    }

    #[test]
    fn the_refusal_says_what_why_nothing_happened_and_what_still_works() {
        let r = screen_refusal(ScreenState::Locked, "mouse_action click").unwrap();
        let err = r.envelope("mouse_action").error.unwrap();
        assert_eq!(err.code, ErrorCode::ActionFailed);
        assert!(err.message.contains("mouse_action click was refused"));
        assert!(err.message.contains("screen is locked"));
        assert!(err.message.contains("discards"));
        assert!(err.message.contains("nothing was done"));
        let s = err.suggestion.unwrap();
        assert!(s.starts_with("retryable:"), "{s}");
        assert!(s.contains("unlock") && s.contains("list_windows") && s.contains("capture"));
    }

    #[test]
    fn a_switched_away_session_is_named_as_such_not_as_a_lock() {
        let r = screen_refusal(ScreenState::OffConsole, "scroll").unwrap();
        assert!(r.message().contains("another user's session"));
        assert!(!r.message().contains("screen is locked"));
        assert!(r.suggestion().contains("switch back"));
    }

    #[test]
    fn lock_hints_combine_without_mistaking_silence_for_unlocked() {
        use ScreenState::*;
        assert_eq!(ScreenState::from_lock_hints(&[]), Unknown);
        assert_eq!(ScreenState::from_lock_hints(&[None, None]), Unknown);
        assert_eq!(ScreenState::from_lock_hints(&[Some(false), None]), Ready);
        assert_eq!(ScreenState::from_lock_hints(&[None, Some(true)]), Locked);
        assert_eq!(
            ScreenState::from_lock_hints(&[Some(false), Some(true)]),
            Locked
        );
    }

    #[test]
    fn labels_are_stable() {
        let all = [
            (ScreenState::Ready, "ok"),
            (ScreenState::Locked, "locked"),
            (ScreenState::OffConsole, "off_console"),
            (ScreenState::Unknown, "unknown"),
        ];
        for (s, l) in all {
            assert_eq!(s.label(), l);
        }
    }
}
