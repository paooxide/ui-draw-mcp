//! The operator's `foreground` setting and the refusal it produces.
//!
//! Lives in the dependency root because three layers have to agree on it: the
//! policy crate parses it, the input and window engines enforce it, and both
//! must say the same thing when they refuse.

use crate::{Envelope, ErrorCode};

/// Whether tools may take the user's pointer, keyboard or foreground.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Foreground {
    /// Today's behaviour: every tool may use synthetic input and raise apps.
    #[default]
    Allow,
    /// No tool moves the real pointer, posts synthetic key or scroll events,
    /// or brings an app or window to the front. Set in the operator's config
    /// or environment only; no tool, role or argument can change it.
    Never,
}

impl Foreground {
    /// The config spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Foreground::Allow => "allow",
            Foreground::Never => "never",
        }
    }

    /// Exact spellings only: an unknown value is a config error, never `Allow`.
    pub fn parse(s: &str) -> Option<Foreground> {
        match s {
            "allow" => Some(Foreground::Allow),
            "never" => Some(Foreground::Never),
            _ => None,
        }
    }

    pub fn is_never(self) -> bool {
        self == Foreground::Never
    }
}

/// A call refused because `foreground = "never"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// What was refused, as a noun phrase ("mouse_action click", "launch").
    pub what: String,
    /// What this would have done to the user's desktop.
    pub effect: &'static str,
    /// The background-safe way to get the same result, when there is one.
    pub instead: Option<String>,
}

impl Refusal {
    pub fn new(what: impl Into<String>, effect: &'static str, instead: Option<&str>) -> Self {
        Refusal {
            what: what.into(),
            effect,
            instead: instead.map(str::to_string),
        }
    }

    /// Says what was refused, why, and that nothing happened.
    pub fn message(&self) -> String {
        format!(
            "{} was refused and nothing was done: this server runs with foreground = \"never\", \
             and this would {}",
            self.what, self.effect
        )
    }

    /// The way forward, or an honest statement that there is none.
    pub fn suggestion(&self) -> String {
        match &self.instead {
            Some(i) => format!("instead: {i}"),
            None => "no background-safe equivalent exists for this; leave this step to the \
                     person at the machine"
                .to_string(),
        }
    }

    /// The tool result. `PolicyDenied`: a policy decided this, as with
    /// `allowed_apps`, so it is audited and counted like any other refusal.
    pub fn envelope(&self, tool: &str) -> Envelope {
        Envelope::fail_with(
            tool,
            ErrorCode::PolicyDenied,
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
    fn only_the_exact_spellings_parse() {
        assert_eq!(Foreground::parse("allow"), Some(Foreground::Allow));
        assert_eq!(Foreground::parse("never"), Some(Foreground::Never));
        for bad in ["", "Never", "NEVER", "deny", "polite", "false", " never"] {
            assert_eq!(Foreground::parse(bad), None, "{bad:?}");
        }
        assert_eq!(Foreground::default(), Foreground::Allow);
    }

    #[test]
    fn a_refusal_says_what_why_and_what_to_do_instead() {
        let r = Refusal::new(
            "mouse_action click",
            "move the real pointer",
            Some("ui_action click"),
        );
        let e = r.envelope("mouse_action");
        let err = e.error.unwrap();
        assert_eq!(err.code, ErrorCode::PolicyDenied);
        assert!(err.message.contains("mouse_action click was refused"));
        assert!(err.message.contains("foreground = \"never\""));
        assert!(err.message.contains("move the real pointer"));
        assert_eq!(err.suggestion.as_deref(), Some("instead: ui_action click"));
    }

    #[test]
    fn with_no_alternative_the_refusal_says_so_rather_than_inventing_one() {
        let r = Refusal::new("drag_drop", "drag with the real pointer", None);
        assert!(r.suggestion().contains("no background-safe equivalent"));
        assert!(!r.suggestion().contains("config"));
    }
}
