//! Whether this login session can receive synthetic input, from the window
//! server's session dictionary.
//!
//! `CGSessionCopyCurrentDictionary` describes the calling process's session:
//! `CGSSessionScreenIsLocked` is present (and true) only while the lock screen
//! is up, and `kCGSSessionOnConsoleKey` is false while another user's session
//! has the console (fast user switching). In both states a posted key or
//! pointer event is accepted by `CGEventPost` and then dropped, so the post
//! gives no sign it failed. Reading the dictionary is a local lookup with no
//! IPC wait, cheap enough to do before every synthetic event.
//!
//! A display that is merely asleep, with the screen not locked, is not treated
//! as blocking: whether a posted event wakes it or is delivered is not
//! documented by Apple, and it could not be measured with the screen locked.
//! Where a lock is required on wake (the usual setting) the session reports
//! locked anyway.

use core_foundation::base::{CFType, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::CFString;
use core_foundation_sys::dictionary::CFDictionaryRef;
use mcp_types::ScreenState;

extern "C" {
    /// Follows the Create rule: the caller owns the returned dictionary. Null
    /// when the process has no window-server session (an SSH login, a daemon).
    fn CGSessionCopyCurrentDictionary() -> CFDictionaryRef;
}

/// The two facts the dictionary carries that matter here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SessionFacts {
    /// `CGSSessionScreenIsLocked`; the key is absent while unlocked.
    pub locked: bool,
    /// `kCGSSessionOnConsoleKey`; a session that does not say is taken to be
    /// the one at the console, since the key's absence has only been seen on
    /// systems old enough not to have fast user switching.
    pub on_console: bool,
}

/// Decide from what the dictionary said. `None` is no dictionary at all.
/// Pure, so the precedence is tested without a screen: a lock is reported as a
/// lock even when the session is also off the console.
pub(crate) fn classify(facts: Option<SessionFacts>) -> ScreenState {
    match facts {
        None => ScreenState::Unknown,
        Some(f) if f.locked => ScreenState::Locked,
        Some(f) if !f.on_console => ScreenState::OffConsole,
        Some(_) => ScreenState::Ready,
    }
}

fn read_facts() -> Option<SessionFacts> {
    let raw = unsafe { CGSessionCopyCurrentDictionary() };
    if raw.is_null() {
        return None;
    }
    let dict: CFDictionary<CFString, CFType> = unsafe { CFDictionary::wrap_under_create_rule(raw) };
    let flag = |key: &str| -> Option<bool> {
        let v = dict.find(CFString::new(key))?;
        v.downcast::<CFBoolean>().map(bool::from)
    };
    Some(SessionFacts {
        locked: flag("CGSSessionScreenIsLocked").unwrap_or(false),
        on_console: flag("kCGSSessionOnConsoleKey").unwrap_or(true),
    })
}

/// The session's state now.
pub fn screen_state() -> ScreenState {
    classify(read_facts())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(locked: bool, on_console: bool) -> Option<SessionFacts> {
        Some(SessionFacts { locked, on_console })
    }

    #[test]
    fn the_four_combinations_and_no_session() {
        assert_eq!(classify(f(false, true)), ScreenState::Ready);
        assert_eq!(classify(f(true, true)), ScreenState::Locked);
        assert_eq!(classify(f(false, false)), ScreenState::OffConsole);
        // Both: the lock is the nearer cause and the one a person can fix.
        assert_eq!(classify(f(true, false)), ScreenState::Locked);
        assert_eq!(classify(None), ScreenState::Unknown);
    }

    /// Against the real window server: on any machine that is logged in at
    /// the console the answer is definite, and a session with no window server
    /// (CI over SSH) says unknown rather than guessing. Passive: it posts
    /// nothing.
    #[test]
    fn the_real_session_gives_an_answer() {
        if std::env::var_os("AGENTCTL_SKIP_LIVE").is_some_and(|v| v != "0") {
            return;
        }
        let s = screen_state();
        eprintln!("session state on this machine: {}", s.label());
        // `screen_state` must agree with itself on a second read.
        assert_eq!(s, screen_state());
    }
}
