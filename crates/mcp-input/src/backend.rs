use async_trait::async_trait;

/// A semantic action on an element (via the accessibility API, no cursor).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticAction {
    Click,
    DoubleClick,
    RightClick,
    Focus,
    Toggle,
    Check,
    Uncheck,
    Expand,
    Collapse,
    Select,
    ScrollIntoView,
}

impl SemanticAction {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "click" => Self::Click,
            "double_click" => Self::DoubleClick,
            "right_click" => Self::RightClick,
            "focus" => Self::Focus,
            "toggle" => Self::Toggle,
            "check" => Self::Check,
            "uncheck" => Self::Uncheck,
            "expand" => Self::Expand,
            "collapse" => Self::Collapse,
            "select" => Self::Select,
            "scroll_into_view" => Self::ScrollIntoView,
            _ => return None,
        })
    }
}

/// A coordinate pointer action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    Move,
    Click,
    Double,
    Triple,
    RightClick,
    Down,
    Up,
}

impl MouseKind {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "move" => Self::Move,
            "click" => Self::Click,
            "double" => Self::Double,
            "triple" => Self::Triple,
            "right_click" => Self::RightClick,
            "down" => Self::Down,
            "up" => Self::Up,
            _ => return None,
        })
    }
}

/// Scroll direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollDir {
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
}

impl ScrollDir {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "up" => Self::Up,
            "down" => Self::Down,
            "left" => Self::Left,
            "right" => Self::Right,
            // Both spellings accepted: `keyboard_shortcut` names these keys
            // `pageup`/`pagedown`, so requiring the underscore only here is a
            // trap. The schema still advertises the canonical form.
            "page_up" | "pageup" => Self::PageUp,
            "page_down" | "pagedown" => Self::PageDown,
            _ => return None,
        })
    }
}

/// Clipboard data format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipFormat {
    Text,
    Html,
    Image,
    Files,
}

impl ClipFormat {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "text" => Self::Text,
            "html" => Self::Html,
            "image" => Self::Image,
            "files" => Self::Files,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Html => "html",
            Self::Image => "image",
            Self::Files => "files",
        }
    }
}

/// Clipboard read result.
#[derive(Debug, Clone)]
pub struct ClipData {
    pub format: ClipFormat,
    pub data: Option<String>,
}

/// Why an input action failed. Mapped to `ErrorCode` by the tool layer.
#[derive(Debug, Clone)]
pub enum InputError {
    PermissionDenied(String),
    /// The element's backend handle is gone (snapshot superseded).
    NotFound(String),
    Unsupported(String),
    /// The caller passed something malformed (e.g. clipboard image data that
    /// is not valid base64).
    InvalidArgs(String),
    Failed(String),
}

/// Platform input backend: semantic AX actions, keyboard, coordinate pointer,
/// and clipboard. macOS (AX + CGEvent) lands behind `macos-backend`.
#[async_trait]
pub trait InputBackend: Send + Sync {
    async fn perform(
        &self,
        node_id: u64,
        action: SemanticAction,
        option: Option<&str>,
    ) -> Result<(), InputError>;
    async fn set_value(&self, node_id: u64, text: &str) -> Result<(), InputError>;
    async fn type_text(&self, text: &str) -> Result<(), InputError>;
    async fn key_combo(&self, combo: &str) -> Result<(), InputError>;
    async fn mouse(
        &self,
        kind: MouseKind,
        x: f64,
        y: f64,
        button: Option<&str>,
        modifiers: &[String],
    ) -> Result<(), InputError>;
    async fn scroll_at(
        &self,
        x: f64,
        y: f64,
        dir: ScrollDir,
        amount: i32,
    ) -> Result<(), InputError>;
    async fn hover(&self, x: f64, y: f64) -> Result<(), InputError>;
    /// Press at `from`, travel to `to` in `steps` intermediate moves, release.
    ///
    /// `since_takeover` is [`InputBackend::takeover_generation`] as read when
    /// the call started; the drag aborts (releasing the button) if it has
    /// moved on by any step.
    ///
    /// The steps are not decoration: a press followed by a single jump is what
    /// a teleport looks like, and drag targets that track motion — Finder
    /// drags, sliders, canvases, reorderable lists — ignore it.
    async fn drag(
        &self,
        from: (f64, f64),
        to: (f64, f64),
        modifiers: &[String],
        steps: u32,
        since_takeover: u64,
    ) -> Result<(), InputError>;
    async fn clipboard_read(&self, format: ClipFormat) -> Result<ClipData, InputError>;
    /// The application that will receive synthetic input *right now*.
    ///
    /// Deliberately not "the app in the last UI snapshot": keystrokes go to the
    /// session's pinned target, or to whatever is frontmost. Those diverge
    /// whenever focus moves, and they diverge permanently for apps that expose
    /// no accessibility tree at all — Electron editors among them — where the
    /// snapshot is simply empty. Any safety check on *what is being typed* has
    /// to ask this, not the snapshot.
    fn input_target(&self) -> Option<String>;
    async fn clipboard_write(&self, format: ClipFormat, data: &str) -> Result<(), InputError>;
    fn platform(&self) -> &'static str;

    /// Where the pointer actually is, in the same coordinate space `mouse`
    /// uses. `None` means this backend cannot tell — which is never treated as
    /// evidence that a human moved it.
    async fn pointer_position(&self) -> Result<Option<(f64, f64)>, InputError> {
        Ok(None)
    }

    /// Why [`Self::pointer_position`] cannot answer, when this backend knows
    /// up front. `None` means it can, or does not know. Lets the watcher say
    /// *why* human-takeover detection is off instead of just that it is.
    fn pointer_unavailable_reason(&self) -> Option<String> {
        None
    }

    /// Pointer positions this backend recently *set*, newest last.
    ///
    /// The watcher compares where the pointer is against where the server put
    /// it; without this it could not tell its own movement from anyone else's.
    fn recent_pointer_sets(&self) -> Vec<crate::SetPoint> {
        Vec::new()
    }

    /// Abandon anything in flight — a drag mid-path, for instance, which would
    /// otherwise keep the button held while the human moves the mouse.
    fn cancel_pending(&self) {}

    /// How many times [`InputBackend::cancel_pending`] has been called, ever.
    ///
    /// A counter rather than a flag: a flag has to be cleared before the next
    /// motion, and whoever clears it also clears a takeover that landed after
    /// the call was admitted but before its first move. With a counter every
    /// motion reads the value when its *call starts* and stops if it changes;
    /// nothing is ever reset, so no takeover can be lost.
    ///
    /// A backend with no cancel support returns 0 forever, which leaves its
    /// motions uninterruptible.
    fn takeover_generation(&self) -> u64 {
        0
    }
}

/// Modifier names accepted by pointer actions, matching `keyboard_shortcut`'s
/// vocabulary so `cmd+click` and `keyboard_shortcut cmd+c` spell "cmd" the
/// same way.
pub fn valid_modifier(name: &str) -> bool {
    matches!(
        name,
        "cmd"
            | "command"
            | "meta"
            | "super"
            | "shift"
            | "opt"
            | "option"
            | "alt"
            | "ctrl"
            | "control"
            | "fn"
    )
}

/// Validate a key combo like `cmd+shift+n`: `^([a-z0-9]+\+)*[a-z0-9]+$`
/// (segments non-empty, ascii alphanumeric, `+`-separated). No regex dependency.
pub fn valid_combo(combo: &str) -> bool {
    if combo.is_empty() {
        return false;
    }
    combo.split('+').all(|seg| {
        !seg.is_empty()
            && seg
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pointer modifiers must accept exactly the names `keyboard_shortcut`
    /// accepts; a vocabulary that differs per tool is a trap.
    #[test]
    fn modifier_vocabulary_matches_the_shortcut_parser() {
        for good in [
            "cmd", "command", "meta", "super", "shift", "opt", "option", "alt", "ctrl", "control",
            "fn",
        ] {
            assert!(valid_modifier(good), "{good} must be accepted");
            assert!(
                valid_combo(&format!("{good}+a")),
                "{good} must parse in a combo"
            );
        }
        for bad in ["Cmd", "windows", "", "cmd+shift"] {
            assert!(!valid_modifier(bad), "{bad} must be rejected");
        }
    }

    /// `scroll` and `keyboard_shortcut` must agree on page-key spelling; an
    /// agent should not have to remember that one wants an underscore.
    #[test]
    fn page_scroll_accepts_both_spellings() {
        assert_eq!(ScrollDir::parse("page_up"), Some(ScrollDir::PageUp));
        assert_eq!(ScrollDir::parse("pageup"), Some(ScrollDir::PageUp));
        assert_eq!(ScrollDir::parse("page_down"), Some(ScrollDir::PageDown));
        assert_eq!(ScrollDir::parse("pagedown"), Some(ScrollDir::PageDown));
        assert_eq!(ScrollDir::parse("sideways"), None);
    }

    #[test]
    fn combos_validate() {
        assert!(valid_combo("cmd+shift+n"));
        assert!(valid_combo("return"));
        assert!(valid_combo("cmd+1"));
        assert!(!valid_combo("cmd+"));
        assert!(!valid_combo("+a"));
        assert!(!valid_combo("Cmd+A")); // must be lowercase
        assert!(!valid_combo(""));
    }
}
