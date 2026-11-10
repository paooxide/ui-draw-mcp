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
    /// Open the element's context menu (AXShowMenu) without a right click.
    ShowMenu,
    /// One step up / down on a slider, stepper or scroll bar.
    Increment,
    Decrement,
    /// Pick the element in a list or picker (AXPick).
    Pick,
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
            // The accessibility API's own name for it, accepted beside ours.
            "scroll_into_view" | "scroll_to_visible" => Self::ScrollIntoView,
            "show_menu" => Self::ShowMenu,
            "increment" => Self::Increment,
            "decrement" => Self::Decrement,
            "pick" => Self::Pick,
            _ => return None,
        })
    }
}

/// How an action reached the UI, so a result can say whether the user's
/// pointer or foreground was involved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// The accessibility API acted on the element: no pointer movement, and
    /// the app need not be frontmost.
    Ax,
    /// Synthetic system input: it moves the real cursor or types into the
    /// frontmost app.
    Hid,
}

impl Route {
    /// The value of a result's `input` field.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ax => "ax",
            Self::Hid => "hid",
        }
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

/// What the OS reports about an element right now: the facts a tool reads
/// back after acting, so "ok" means the UI changed and not merely that a call
/// returned.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reading {
    /// The text or value the control shows. `None` when the platform cannot
    /// say, which callers treat as "unverifiable", never as "empty".
    pub value: Option<String>,
    /// Checked state for checkboxes, switches and radios. `None` when the
    /// element has none or it cannot be read (a mixed checkbox included).
    pub checked: Option<bool>,
}

/// A text selection as the element reports it.
///
/// Offsets are characters (Unicode scalar values) from the start of the
/// element's text, `end` exclusive, whatever unit the platform counts in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub start: usize,
    pub end: usize,
    /// The selected text, as the element reports it.
    pub text: String,
    /// The element's role, when the backend knows it. Useful when the element
    /// was the focused one rather than a named ref.
    pub role: Option<String>,
}

/// The outcome of choosing an option from a popup button or combo box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// The entry that was matched, as the control spells it.
    pub item: String,
    /// What the control shows after the choice, read back from the OS.
    /// `None` when it could not be read, in which case the choice is unverified.
    pub selected: Option<String>,
    /// Whether the control showed something different before.
    pub changed: bool,
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
    /// Refused by the operator's `foreground = "never"` before anything was
    /// done. Mapped to `POLICY_DENIED`: a policy decided this, not the OS.
    Refused(String),
}

/// Platform input backend: semantic AX actions, keyboard, coordinate pointer,
/// and clipboard. macOS (AX + CGEvent) lands behind `macos-backend`.
#[async_trait]
pub trait InputBackend: Send + Sync {
    /// Act on an element and say which route did it. A backend returns
    /// [`Route::Hid`] only when it had to fall back to synthetic input; the
    /// tool layer reports exactly what it is told and never assumes.
    async fn perform(
        &self,
        node_id: u64,
        action: SemanticAction,
        option: Option<&str>,
    ) -> Result<Route, InputError>;
    /// The route [`InputBackend::perform`] will take for `action`, known before
    /// it runs. `foreground = "never"` has to refuse a pointer click *before*
    /// it is posted, and the route a call reports only exists afterwards.
    ///
    /// Required, with no default, so a backend cannot be added without saying
    /// which of its actions need the pointer. It must agree with what
    /// `perform` returns; answer [`Route::Hid`] when unsure.
    fn route_for(&self, action: SemanticAction) -> Route;
    /// Choose the entry named `option` from a popup button or combo box, or
    /// from the popup an entry belongs to when `node_id` is itself an entry.
    ///
    /// Opens the popup, finds the entry, activates it, reads back what the
    /// control shows, and closes anything it opened if it fails. A backend
    /// that cannot do this says `Unsupported`, and the caller falls back to a
    /// plain `perform`.
    async fn choose_option(&self, _node_id: u64, _option: &str) -> Result<Choice, InputError> {
        Err(InputError::Unsupported(
            "choosing an option by text is not supported on this platform".into(),
        ))
    }
    /// Read an element's current value and checked state from the OS (not the
    /// snapshot, which is as old as the last observation).
    async fn read_element(&self, _node_id: u64) -> Result<Reading, InputError> {
        Ok(Reading::default())
    }
    /// Write an element's value through the accessibility API. Always
    /// [`Route::Ax`]: an implementation that would have to type must refuse.
    async fn set_value(&self, node_id: u64, text: &str) -> Result<(), InputError>;
    /// The current text selection of `node_id`, or of the focused element when
    /// `None`, without changing it.
    async fn selection(&self, _node_id: Option<u64>) -> Result<Selection, InputError> {
        Err(InputError::Unsupported(
            "reading a text selection is not supported on this platform".into(),
        ))
    }
    /// Select characters `start..end` (see [`Selection`]) of an element's text
    /// through the accessibility API and return the selection the element then
    /// reports. Never drags the pointer; an element that will not take the
    /// range is `Unsupported`, not approximated.
    async fn select_range(
        &self,
        _node_id: u64,
        _start: usize,
        _end: usize,
    ) -> Result<Selection, InputError> {
        Err(InputError::Unsupported(
            "selecting text is not supported on this platform".into(),
        ))
    }
    /// Synthetic system input follows: `type_text`, `key_combo`, `mouse`,
    /// `scroll_at`, `hover` and `drag` all move the real pointer or type into
    /// the frontmost app, and the tool layer reports them as [`Route::Hid`].
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
    /// Turn the wheel at `(x, y)`: `amount` lines (ten per page) in `dir`,
    /// with `modifiers` held for the whole gesture, so Cmd+wheel or Ctrl+wheel
    /// zooms and Shift+wheel scrolls sideways where the app binds them. Same
    /// vocabulary as [`InputBackend::mouse`].
    async fn scroll_at(
        &self,
        x: f64,
        y: f64,
        dir: ScrollDir,
        amount: i32,
        modifiers: &[String],
    ) -> Result<(), InputError>;
    async fn hover(&self, x: f64, y: f64) -> Result<(), InputError>;
    /// Press at `from`, move to each point of `route` in order, release on the
    /// last one.
    ///
    /// `route` is never empty and is already spaced: the tool layer spreads
    /// the moves (a straight line, or a path through waypoints) so the
    /// backends only play them back. The steps are not decoration: a press
    /// followed by a single jump is what a teleport looks like, and drag
    /// targets that track motion (Finder drags, sliders, canvases, reorderable
    /// lists) ignore it.
    ///
    /// `since_takeover` is [`InputBackend::takeover_generation`] as read when
    /// the call started; the drag aborts (releasing the button) if it has
    /// moved on by any step.
    ///
    /// `hold_ms` is how long the button stays down before the first move, on
    /// top of the short settle every drag gets. Drag sources that wait to see
    /// whether a press is a click (Finder icons, list rows) need it.
    async fn drag(
        &self,
        from: (f64, f64),
        route: &[(f64, f64)],
        modifiers: &[String],
        hold_ms: u64,
        since_takeover: u64,
    ) -> Result<(), InputError>;
    /// Press `key` (a canonical name from [`crate::parse_hold`]: a modifier or
    /// one key) and leave it down until [`InputBackend::key_up`].
    ///
    /// While any key is down, every pointer, scroll and key-chord event this
    /// backend posts carries the held modifiers, so a click while Shift is
    /// held is a shift-click whatever modifiers the call itself named. Holding
    /// a key that is already down is not an error.
    async fn key_down(&self, key: &str) -> Result<(), InputError>;
    /// Release a key pressed with [`InputBackend::key_down`]. Releasing one
    /// that is not down is not an error. Must not refuse for any reason other
    /// than a failure to post the event: a release that can be vetoed is a
    /// stuck key.
    async fn key_up(&self, key: &str) -> Result<(), InputError>;
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
                crate::combo::parse_combo(&format!("{good}+a"), crate::combo::Os::Mac).is_ok(),
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
    fn the_new_actions_parse_in_either_spelling() {
        assert_eq!(
            SemanticAction::parse("show_menu"),
            Some(SemanticAction::ShowMenu)
        );
        assert_eq!(
            SemanticAction::parse("increment"),
            Some(SemanticAction::Increment)
        );
        assert_eq!(
            SemanticAction::parse("decrement"),
            Some(SemanticAction::Decrement)
        );
        assert_eq!(SemanticAction::parse("pick"), Some(SemanticAction::Pick));
        assert_eq!(
            SemanticAction::parse("scroll_to_visible"),
            Some(SemanticAction::ScrollIntoView)
        );
        assert_eq!(SemanticAction::parse("showmenu"), None);
    }

    #[test]
    fn a_route_is_spelled_the_way_a_result_reports_it() {
        assert_eq!(Route::Ax.as_str(), "ax");
        assert_eq!(Route::Hid.as_str(), "hid");
    }
}
