//! Clipboard, with a fallback for compositors that withhold the Wayland
//! clipboard protocol.
//!
//! `wl-clipboard-rs` speaks `wlr-data-control`/`ext-data-control`, the
//! protocol that lets a client with no focused surface read and write the
//! selection. Mutter (GNOME) does not implement it, so on GNOME the direct
//! path fails with "a required Wayland protocol is not supported". GNOME does
//! bridge the X11 clipboard to the Wayland one through XWayland, so an X11
//! tool (`xclip` or `xsel`) reaches the same clipboard when `DISPLAY` is set.
//! Where neither the protocol nor an X11 tool is available, clipboard access
//! is honestly unavailable, and the error says which and how to fix it.

use std::path::Path;

use mcp_input::ClipFormat;

/// The MIME type a format maps to. Every format has one: an image rides the
/// clipboard as PNG, and a file list as `text/uri-list` (the freedesktop
/// convention, one `file://` URI per line).
pub fn mime_for(format: ClipFormat) -> &'static str {
    match format {
        ClipFormat::Text => "text/plain",
        ClipFormat::Html => "text/html",
        ClipFormat::Image => "image/png",
        ClipFormat::Files => "text/uri-list",
    }
}

/// Is a `wl-clipboard-rs` error the "compositor lacks the protocol" one, as
/// opposed to an empty clipboard or a genuine fault? The crate surfaces it
/// only as message text.
pub fn missing_protocol(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    (m.contains("data-control") || m.contains("required wayland protocol"))
        && m.contains("not supported")
}

/// An X11 clipboard tool usable over XWayland.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum X11Tool {
    Xclip,
    Xsel,
}

impl X11Tool {
    /// The first tool present on `PATH`, when an X display exists. `None`
    /// means the XWayland fallback is not available.
    pub fn detect() -> Option<(X11Tool, String)> {
        // No X display, no XWayland bridge.
        std::env::var_os("DISPLAY")?;
        for (tool, bin) in [(X11Tool::Xclip, "xclip"), (X11Tool::Xsel, "xsel")] {
            for dir in ["/usr/bin", "/usr/local/bin", "/bin"] {
                let p = Path::new(dir).join(bin);
                if p.is_file() {
                    return Some((tool, p.display().to_string()));
                }
            }
        }
        None
    }

    /// Whether this tool can carry `format`. `xclip` speaks arbitrary MIME
    /// types, so it handles all four; `xsel` only ever touches the text
    /// selection, so an image or a file list has no representation through it.
    pub fn supports(self, format: ClipFormat) -> bool {
        match self {
            X11Tool::Xclip => true,
            X11Tool::Xsel => matches!(format, ClipFormat::Text | ClipFormat::Html),
        }
    }

    /// Argv to read the clipboard as `mime`. `xsel` ignores the MIME type,
    /// so HTML comes back as text.
    pub fn read_args(self, mime: &str) -> Vec<String> {
        match self {
            X11Tool::Xclip => vec![
                "-selection".into(),
                "clipboard".into(),
                "-o".into(),
                "-t".into(),
                mime.into(),
            ],
            X11Tool::Xsel => vec!["-b".into(), "-o".into()],
        }
    }

    /// Argv to write the clipboard as `mime`. Data goes on stdin.
    pub fn write_args(self, mime: &str) -> Vec<String> {
        match self {
            X11Tool::Xclip => vec![
                "-selection".into(),
                "clipboard".into(),
                "-i".into(),
                "-t".into(),
                mime.into(),
            ],
            X11Tool::Xsel => vec!["-b".into(), "-i".into()],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_protocol_error_is_told_apart_from_ordinary_failures() {
        assert!(missing_protocol(
            "A required Wayland protocol (ext-data-control, or wlr-data-control version 1) is not supported by the compositor"
        ));
        assert!(missing_protocol(
            "clipboard write: data-control is not supported"
        ));
        assert!(!missing_protocol("clipboard is empty"));
        assert!(!missing_protocol("connection reset"));
        assert!(!missing_protocol("no seats"));
    }

    #[test]
    fn every_format_maps_to_a_mime_type() {
        assert_eq!(mime_for(ClipFormat::Text), "text/plain");
        assert_eq!(mime_for(ClipFormat::Html), "text/html");
        assert_eq!(mime_for(ClipFormat::Image), "image/png");
        assert_eq!(mime_for(ClipFormat::Files), "text/uri-list");
    }

    #[test]
    fn xsel_carries_only_text_but_xclip_carries_everything() {
        for f in [
            ClipFormat::Text,
            ClipFormat::Html,
            ClipFormat::Image,
            ClipFormat::Files,
        ] {
            assert!(X11Tool::Xclip.supports(f), "xclip should carry {f:?}");
        }
        assert!(X11Tool::Xsel.supports(ClipFormat::Text));
        assert!(X11Tool::Xsel.supports(ClipFormat::Html));
        assert!(!X11Tool::Xsel.supports(ClipFormat::Image));
        assert!(!X11Tool::Xsel.supports(ClipFormat::Files));
    }

    #[test]
    fn tool_argv_selects_the_clipboard_and_passes_the_mime() {
        assert_eq!(
            X11Tool::Xclip.read_args("text/html"),
            vec!["-selection", "clipboard", "-o", "-t", "text/html"]
        );
        assert_eq!(
            X11Tool::Xclip.write_args("text/plain"),
            vec!["-selection", "clipboard", "-i", "-t", "text/plain"]
        );
        // xsel is text-only: the MIME type does not appear.
        assert_eq!(X11Tool::Xsel.read_args("text/html"), vec!["-b", "-o"]);
        assert_eq!(X11Tool::Xsel.write_args("text/plain"), vec!["-b", "-i"]);
    }
}
