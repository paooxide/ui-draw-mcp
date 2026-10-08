//! Real macOS backend: AXUIElement perception + semantic AX actions, shared so
//! `get_ui_tree` refs (from `mcp-a11y`) can be acted on by `mcp-input`.
//!
//! Requires the **Accessibility** TCC permission for the launching app; without
//! it, every call returns `PERM_DENIED` (which is verifiable headlessly). Live
//! behavior (reading real apps, performing actions) can only be validated on a
//! GUI session with the permission granted.
//!
//! Coordinate/keyboard/clipboard input needs CGEvent/NSPasteboard and is not yet
//! implemented here (returns `UNSUPPORTED_OS`); semantic actions (AXPress,
//! set-value, focus) are real. On non-macOS targets this crate is empty.

// Pure logic, so it is also compiled for tests on other hosts.
#[cfg(any(target_os = "macos", test))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod chunk;
#[cfg(target_os = "macos")]
mod event;
#[cfg(target_os = "macos")]
mod imp;
#[cfg(target_os = "macos")]
mod ocr;
#[cfg(target_os = "macos")]
mod popup;
#[cfg(target_os = "macos")]
mod vision;
#[cfg(target_os = "macos")]
pub use imp::{permissions, MacosBackend, Permissions};

#[cfg(all(test, target_os = "macos"))]
mod permission_tests {
    /// Both grants fail silently when missing — AX returns nothing, capture
    /// returns the wallpaper — so the check must be a real preflight rather
    /// than something that only reports after a failure.
    #[test]
    fn permissions_report_without_prompting() {
        let p = super::permissions();
        // Whatever the answer is, it must be an answer: the assertion is that
        // the call returns rather than blocking on a system dialog.
        let _ = (p.accessibility, p.screen_recording);
        if p.accessibility {
            // A trusted process must be able to reach the window list.
            let b = super::MacosBackend::new();
            assert_eq!(mcp_window::WindowBackend::platform(&b), "macos");
        }
    }
}
