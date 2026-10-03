//! Reading the pointer position, which only an X11 session can answer.
//!
//! The human-takeover watcher compares where the pointer is against where
//! agentctl last put it, so it needs a global pointer query. X11 has one
//! (`QueryPointer`); Wayland deliberately has none. XWayland is *not* a
//! substitute: it reports the pointer only while it is over an X11 window and
//! keeps the last value after it leaves, which the watcher would read as a
//! human moving the mouse and trip the kill switch on nothing.
//!
//! No X11 client crate is in the dependency tree, so this shells out to
//! `xdotool getmouselocation`, the same way the clipboard fallback shells out
//! to `xclip`. The coordinates are X root coordinates, which match the
//! coordinates `mouse` uses on a single-monitor setup; with several monitors
//! the portal's per-stream coordinates can differ from the root's, and the
//! watcher may then see its own moves as foreign.

use std::path::Path;
use std::sync::OnceLock;

/// Whether this session can answer a pointer query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PointerSupport {
    /// An X11 session with `xdotool` at this path.
    X11 { xdotool: String },
    /// No usable query; the string says why, for a log line or `doctor`.
    Unavailable(String),
}

/// Decide from the environment. Pure, so every branch is testable without a
/// display.
pub fn classify(
    session_type: &str,
    display: Option<&str>,
    wayland_display: Option<&str>,
    xdotool: Option<String>,
) -> PointerSupport {
    let wayland = match session_type {
        "x11" => false,
        "wayland" => true,
        // tty, unset or unknown: believe a Wayland socket if there is one.
        _ => wayland_display.is_some_and(|w| !w.is_empty()),
    };
    if wayland {
        return PointerSupport::Unavailable(
            "Wayland session: the compositor offers no global pointer query, and XWayland only \
             reports the pointer while it is over an X11 window"
                .into(),
        );
    }
    if display.map_or(true, str::is_empty) {
        return PointerSupport::Unavailable("no DISPLAY, so there is no X server to ask".into());
    }
    match xdotool {
        Some(path) => PointerSupport::X11 { xdotool: path },
        None => PointerSupport::Unavailable(
            "X11 session, but xdotool is not installed (install xdotool)".into(),
        ),
    }
}

/// The session's support, probed once.
pub fn support() -> &'static PointerSupport {
    static SUPPORT: OnceLock<PointerSupport> = OnceLock::new();
    SUPPORT.get_or_init(|| {
        classify(
            &std::env::var("XDG_SESSION_TYPE").unwrap_or_default(),
            std::env::var("DISPLAY").ok().as_deref(),
            std::env::var("WAYLAND_DISPLAY").ok().as_deref(),
            find_xdotool(),
        )
    })
}

fn find_xdotool() -> Option<String> {
    ["/usr/bin", "/usr/local/bin", "/bin"]
        .iter()
        .map(|d| Path::new(d).join("xdotool"))
        .find(|p| p.is_file())
        .map(|p| p.display().to_string())
}

/// Parse `xdotool getmouselocation --shell` output (`X=1\nY=2\nSCREEN=0\n...`).
pub fn parse_mouse_location(out: &str) -> Option<(f64, f64)> {
    let mut x = None;
    let mut y = None;
    for line in out.lines() {
        match line.trim().split_once('=') {
            Some(("X", v)) => x = v.trim().parse::<i64>().ok(),
            Some(("Y", v)) => y = v.trim().parse::<i64>().ok(),
            _ => {}
        }
    }
    Some((x? as f64, y? as f64))
}

/// Ask the X server where the pointer is. Needs a display; not unit-tested.
pub async fn query(xdotool: &str) -> Result<Option<(f64, f64)>, String> {
    let run = tokio::process::Command::new(xdotool)
        .args(["getmouselocation", "--shell"])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(std::time::Duration::from_millis(500), run)
        .await
        .map_err(|_| "xdotool getmouselocation timed out".to_string())?
        .map_err(|e| format!("{xdotool}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "xdotool getmouselocation failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(parse_mouse_location(&String::from_utf8_lossy(&out.stdout)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_shell_form() {
        let out = "X=1204\nY=87\nSCREEN=0\nWINDOW=60817414\n";
        assert_eq!(parse_mouse_location(out), Some((1204.0, 87.0)));
    }

    #[test]
    fn negative_coordinates_and_missing_fields() {
        assert_eq!(parse_mouse_location("X=-5\nY=10\n"), Some((-5.0, 10.0)));
        assert_eq!(parse_mouse_location("X=5\nSCREEN=0\n"), None);
        assert_eq!(parse_mouse_location("X=a\nY=2\n"), None);
        assert_eq!(parse_mouse_location(""), None);
    }

    fn tool() -> Option<String> {
        Some("/usr/bin/xdotool".into())
    }

    #[test]
    fn an_x11_session_with_xdotool_is_supported() {
        assert_eq!(
            classify("x11", Some(":0"), None, tool()),
            PointerSupport::X11 {
                xdotool: "/usr/bin/xdotool".into()
            }
        );
    }

    #[test]
    fn wayland_is_unavailable_even_with_xwayland_and_xdotool() {
        let s = classify("wayland", Some(":0"), Some("wayland-0"), tool());
        assert!(matches!(&s, PointerSupport::Unavailable(r) if r.contains("Wayland")));
    }

    #[test]
    fn an_unset_session_type_falls_back_on_the_wayland_socket() {
        assert!(matches!(
            classify("", Some(":0"), Some("wayland-0"), tool()),
            PointerSupport::Unavailable(_)
        ));
        assert!(matches!(
            classify("tty", Some(":0"), None, tool()),
            PointerSupport::X11 { .. }
        ));
    }

    #[test]
    fn x11_without_a_display_or_xdotool_says_which() {
        let no_display = classify("x11", None, None, tool());
        assert!(matches!(&no_display, PointerSupport::Unavailable(r) if r.contains("DISPLAY")));
        let no_tool = classify("x11", Some(":0"), None, None);
        assert!(matches!(&no_tool, PointerSupport::Unavailable(r) if r.contains("xdotool")));
    }
}
