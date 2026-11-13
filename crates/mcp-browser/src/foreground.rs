//! `foreground = "never"` and `"polite"` for the browser engine: which
//! operations can raise a window or take focus from the person, and what each
//! becomes.
//!
//! The browser is driven over DevTools, which never moves the pointer or posts
//! system input, so only a visible (headed) Chrome is a concern, and only for
//! a handful of operations: starting one, bringing a page to the front, and
//! creating or activating a tab. Under `never` each of those is made
//! background-safe where the protocol has a form for it and refused where it
//! does not, naming the headless option. A page that needs to believe it has
//! focus (`document.hasFocus()`, `:focus`, focus and blur handlers) does not
//! need the window in front: `Emulation.setFocusEmulationEnabled`, which the
//! engine already sets on every session that runs page script, is the
//! substitute.
//!
//! `polite` borrows the desktop for a call and gives the frontmost app back,
//! which lives in the desktop engines. This engine has no input backend to
//! borrow through and a window a browser raises stays raised, so under
//! `polite` it applies `never`'s rule and says `polite` in the refusal.
//! Under `allow` nothing here changes anything.

use mcp_types::{Foreground, Refusal};

/// An operation that can raise a window or take focus when Chrome is headed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Raise {
    /// `Page.bringToFront` on connect, so the page the person sees is the one
    /// being driven.
    BringToFront,
    /// The same, when a screencast starts.
    ScreencastToFront,
    /// Starting a new Chromium. A headed one opens a window and the OS
    /// activates the app.
    LaunchChromium { headless: bool },
    /// Starting Safari through safaridriver, which opens and activates Safari.
    LaunchSafari,
    /// `browser_tabs open`: `/json/new` creates the tab and activates it.
    OpenTab,
    /// A tab in a fresh browser context for a branch: a window of its own.
    OpenBranchTab,
    /// `browser_tabs activate`: `/json/activate` raises the tab's window.
    ActivateTab { headless: bool },
}

/// What an operation becomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Exactly as it was before the setting existed.
    AsBefore,
    /// Do not do it. Nothing replaces it here: the focus emulation every
    /// scripted session sets is what lets a page behind others see focus.
    Skip,
    /// Do it through the form that does not raise: `Target.createTarget`
    /// with `background: true`.
    Background,
    /// Refuse with `POLICY_DENIED`, nothing done.
    Refuse(Refusal),
}

const HEADLESS: &str = "launch with `headless: true`, which has no window and is unaffected, or \
                        attach to a Chromium that is already running (`attach.port`)";

/// Decide what `op` becomes under `fg`. Pure, so the table is tested without
/// a browser.
pub fn plan(fg: Foreground, op: Raise) -> Plan {
    if fg == Foreground::Allow {
        return Plan::AsBefore;
    }
    let refuse = |what: &str, effect: &'static str, instead: &str| {
        Plan::Refuse(Refusal::new(what, effect, Some(instead)).under(fg))
    };
    match op {
        Raise::BringToFront | Raise::ScreencastToFront => Plan::Skip,
        Raise::OpenTab | Raise::OpenBranchTab => Plan::Background,
        Raise::LaunchChromium { headless: true } => Plan::AsBefore,
        Raise::LaunchChromium { headless: false } => refuse(
            "browser_connect launch of a visible browser",
            "open a browser window and make its app the frontmost one, and a new Chromium \
             cannot be started without the OS activating it",
            HEADLESS,
        ),
        Raise::LaunchSafari => refuse(
            "browser_connect launch of Safari",
            "open a Safari window and make Safari the frontmost app, and Safari has no \
             headless mode",
            "launch Chromium with `headless: true`, or attach to a Chromium that is already \
             running (`attach.port`)",
        ),
        Raise::ActivateTab { headless: true } => Plan::AsBefore,
        Raise::ActivateTab { headless: false } => refuse(
            "browser_tabs activate",
            "raise the tab's window above the person's apps",
            "pass `target_id` to the browser tools instead: every one of them acts on the tab \
             it names, in the background, without activating it",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Raise; 8] = [
        Raise::BringToFront,
        Raise::ScreencastToFront,
        Raise::LaunchChromium { headless: false },
        Raise::LaunchChromium { headless: true },
        Raise::LaunchSafari,
        Raise::OpenTab,
        Raise::OpenBranchTab,
        Raise::ActivateTab { headless: false },
    ];

    #[test]
    fn allow_changes_nothing() {
        for op in ALL
            .into_iter()
            .chain([Raise::ActivateTab { headless: true }])
        {
            assert_eq!(plan(Foreground::Allow, op), Plan::AsBefore, "{op:?}");
        }
    }

    #[test]
    fn never_and_polite_decide_alike() {
        for op in ALL
            .into_iter()
            .chain([Raise::ActivateTab { headless: true }])
        {
            let strip = |p: Plan| match p {
                Plan::Refuse(r) => Plan::Refuse(Refusal::new(r.what, r.effect, None)),
                other => other,
            };
            assert_eq!(
                strip(plan(Foreground::Never, op)),
                strip(plan(Foreground::Polite, op)),
                "{op:?}"
            );
        }
    }

    #[test]
    fn a_page_is_not_brought_forward_and_tabs_open_in_the_background() {
        for fg in [Foreground::Never, Foreground::Polite] {
            assert_eq!(plan(fg, Raise::BringToFront), Plan::Skip);
            assert_eq!(plan(fg, Raise::ScreencastToFront), Plan::Skip);
            assert_eq!(plan(fg, Raise::OpenTab), Plan::Background);
            assert_eq!(plan(fg, Raise::OpenBranchTab), Plan::Background);
        }
    }

    #[test]
    fn headless_is_untouched_because_it_has_no_window_to_raise() {
        for fg in [Foreground::Never, Foreground::Polite] {
            assert_eq!(
                plan(fg, Raise::LaunchChromium { headless: true }),
                Plan::AsBefore
            );
            assert_eq!(
                plan(fg, Raise::ActivateTab { headless: true }),
                Plan::AsBefore
            );
        }
    }

    #[test]
    fn a_visible_launch_or_activation_is_refused_naming_the_way_out() {
        let refusal = |p: Plan| match p {
            Plan::Refuse(r) => r,
            other => panic!("not refused: {other:?}"),
        };
        let r = refusal(plan(
            Foreground::Never,
            Raise::LaunchChromium { headless: false },
        ));
        assert!(r.message().contains("foreground = \"never\""));
        assert!(
            r.suggestion().contains("headless: true"),
            "{}",
            r.suggestion()
        );
        assert!(r.suggestion().contains("attach.port"));
        let r = refusal(plan(Foreground::Polite, Raise::LaunchSafari));
        assert!(
            r.message().contains("foreground = \"polite\""),
            "{}",
            r.message()
        );
        assert!(r.suggestion().contains("headless: true"));
        let r = refusal(plan(
            Foreground::Never,
            Raise::ActivateTab { headless: false },
        ));
        assert!(r.suggestion().contains("target_id"));
        let e = r.envelope("browser_tabs");
        assert_eq!(e.error.unwrap().code, mcp_types::ErrorCode::PolicyDenied);
    }
}
