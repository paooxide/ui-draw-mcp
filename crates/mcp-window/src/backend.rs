use async_trait::async_trait;
use serde::Serialize;

/// A rectangle in screen coordinates.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Where a window is, as far as the window server says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WindowState {
    /// Drawn on a display now (it may still be behind other windows).
    Onscreen,
    /// Minimised to the Dock or taskbar.
    Minimized,
    /// Not drawn and not known to be minimised: on another Space or
    /// workspace, or its application is hidden.
    Offscreen,
}

/// A window, as observed by the backend.
#[derive(Debug, Clone, Serialize)]
pub struct WindowInfo {
    /// Index within the window list of the application named in `app`. It
    /// means the same thing whether the list was asked for one application or
    /// for all of them, and every tool that takes a `window_id` reads it
    /// against the application it is told about (`app`, else the pinned one),
    /// never against whichever is frontmost.
    pub id: u32,
    pub app: Option<String>,
    /// The owning process, so two applications with one name stay apart.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub title: Option<String>,
    pub bounds: Option<Rect>,
    pub minimized: bool,
    pub state: WindowState,
}

/// How a call came to act on the application it acted on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetSource {
    /// The call named it (`app`).
    Named,
    /// The session was pinned to it (`focus_app`, or an earlier `get_ui_tree`
    /// with `app`) and the call named no other.
    Pinned,
    /// Nothing named or pinned: the frontmost application at that moment.
    Frontmost,
}

impl TargetSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Named => "named",
            Self::Pinned => "pinned",
            Self::Frontmost => "frontmost",
        }
    }
}

/// The application a call acts on, and why that one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TargetInfo {
    pub app: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub from: TargetSource,
}

/// Which source decides the target: a name given to the call wins (an empty
/// or blank name counts as none), then a pin that is still alive, then the
/// frontmost application. Every backend applies this one rule, so a result's
/// `from` means the same on each platform.
pub fn target_source(named: Option<&str>, pinned_alive: bool) -> TargetSource {
    if named.is_some_and(|n| !n.trim().is_empty()) {
        TargetSource::Named
    } else if pinned_alive {
        TargetSource::Pinned
    } else {
        TargetSource::Frontmost
    }
}

/// The error for a `window_id` the application does not have. It names the
/// application and how many windows it has, because the usual cause is an id
/// taken from another application's list, and a bare "not found" would send
/// the caller looking for the window instead of for the mix-up.
pub fn window_index_error(app: &str, pid: Option<u32>, id: u32, count: usize) -> String {
    let who = match pid {
        Some(p) => format!("'{app}' (pid {p})"),
        None => format!("'{app}'"),
    };
    match count {
        0 => format!("{who} has no windows, so there is no window_id {id}"),
        1 => format!(
            "{who} has 1 window (window_id 0); there is no window_id {id}. A window_id is read \
             against one application: pass the app it came from"
        ),
        n => format!(
            "{who} has {n} windows (window_id 0 to {}); there is no window_id {id}. A window_id \
             is read against one application: pass the app it came from",
            n - 1
        ),
    }
}

/// `windows[id]`, or the error [`window_index_error`] words.
pub fn window_at<'a>(
    windows: &'a [WindowInfo],
    app: &str,
    pid: Option<u32>,
    id: u32,
) -> Result<&'a WindowInfo, String> {
    windows
        .get(id as usize)
        .ok_or_else(|| window_index_error(app, pid, id, windows.len()))
}

/// A window-control verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowAction {
    Focus,
    Minimize,
    Maximize,
    Restore,
    Close,
    Move,
    Resize,
}

impl WindowAction {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "focus" => Self::Focus,
            "minimize" => Self::Minimize,
            "maximize" => Self::Maximize,
            "restore" => Self::Restore,
            "close" => Self::Close,
            "move" => Self::Move,
            "resize" => Self::Resize,
            _ => return None,
        })
    }
}

/// How wide to cast when looking for dialogs.
///
/// `App` inspects one application. `System` sweeps every process that owns an
/// on-screen window, which is the only way to see an authentication or
/// permission prompt: on macOS those are raised by a *separate* process
/// (`SecurityAgent`, `UserNotificationCenter`), so they never appear in the
/// window list of the app that triggered them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogScope {
    App,
    System,
}

impl DialogScope {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "app" => Self::App,
            "system" => Self::System,
            _ => return None,
        })
    }
}

/// A modal-ish thing asking for a decision: a dialog window, a sheet attached
/// to a window, a popover, or an open menu.
///
/// Unlike [`WindowInfo`] this carries the *choices* — button titles, which one
/// Return activates, which one Escape activates — so an agent can decide what
/// to do in one round trip instead of pulling a whole UI tree first.
#[derive(Debug, Clone, Serialize)]
pub struct DialogInfo {
    /// Index within this result set (not stable across calls).
    pub id: u32,
    pub app: Option<String>,
    pub title: Option<String>,
    /// `dialog` | `alert` | `sheet` | `popover` | `menu`.
    pub kind: String,
    pub bounds: Option<Rect>,
    /// Button titles in tree order — the choices on offer.
    pub buttons: Vec<String>,
    /// Static text inside the dialog: the question being asked.
    pub text: Vec<String>,
    /// The button Return activates. Usually the non-destructive one.
    pub default_button: Option<String>,
    /// The button Escape activates.
    pub cancel_button: Option<String>,
    /// True when the dialog contains a secure (password) field.
    ///
    /// This is a stop signal, not an invitation: the agent should hand control
    /// back to the human rather than attempt to fill it. The field's value is
    /// never readable — see the a11y engine's secure-field redaction.
    pub has_secure_field: bool,
}

/// One entry in an application's menu bar.
#[derive(Debug, Clone, Serialize)]
pub struct MenuItemInfo {
    /// Full title path from the menu bar, e.g. `["File", "Save As\u{2026}"]`.
    pub path: Vec<String>,
    pub title: String,
    pub enabled: bool,
    /// Whether this item opens a submenu.
    pub has_submenu: bool,
    /// Keyboard equivalent in `keyboard_shortcut` syntax (e.g. `cmd+shift+s`)
    /// when the item advertises one. Cheaper and more reliable than walking
    /// the menu with clicks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shortcut: Option<String>,
}

/// Why a window operation failed.
#[derive(Debug, Clone)]
pub enum WindowError {
    PermissionDenied(String),
    NotFound(String),
    Unsupported(String),
    Failed(String),
    /// Refused by the operator's `foreground = "never"` before anything was
    /// done. Mapped to `POLICY_DENIED`: a policy decided this, not the OS.
    Refused(String),
}

/// Platform window/app/menu backend.
#[async_trait]
pub trait WindowBackend: Send + Sync {
    /// Windows of an app (or the focused app when `app` is `None`).
    async fn list_windows(&self, app: Option<&str>) -> Result<Vec<WindowInfo>, WindowError>;
    /// Names of running apps.
    async fn list_apps(&self) -> Result<Vec<String>, WindowError>;
    async fn launch(&self, app: &str) -> Result<(), WindowError>;
    /// Start `app` without asking the OS to bring it forward. A platform with
    /// no such request says so, and `foreground = "never"` then refuses the
    /// launch rather than activate the app.
    ///
    /// The app may still raise itself once it is running; this only declines
    /// to ask.
    async fn launch_in_background(&self, _app: &str) -> Result<(), WindowError> {
        Err(WindowError::Refused(
            mcp_types::Refusal::new(
                "launch",
                "start an app and bring it to the front: agentctl has no way to launch one \
                 without activating it on this platform",
                None,
            )
            .one_line(),
        ))
    }
    async fn close_app(&self, target: &str) -> Result<(), WindowError>;
    async fn control_window(
        &self,
        app: Option<&str>,
        title: Option<&str>,
        action: WindowAction,
        bounds: Option<Rect>,
    ) -> Result<(), WindowError>;
    /// Whether [`WindowBackend::control_window`] brings the window forward to
    /// do `action` on this platform. Required, with no default, so a backend
    /// cannot be added without answering; answer `true` when unsure.
    fn control_takes_foreground(&self, action: WindowAction) -> bool;
    /// Open a menu-bar menu by title path (does not click a leaf).
    async fn menu_open(&self, app: Option<&str>, path: &[String]) -> Result<(), WindowError>;
    /// Open and click a menu item by title path.
    async fn menu_invoke(&self, app: Option<&str>, path: &[String]) -> Result<(), WindowError>;
    /// Open dialogs, sheets, popovers and menus.
    async fn list_dialogs(
        &self,
        app: Option<&str>,
        scope: DialogScope,
    ) -> Result<Vec<DialogInfo>, WindowError>;
    /// Enumerate an application's menu bar down to `depth` levels.
    async fn menu_list(
        &self,
        app: Option<&str>,
        path: &[String],
        depth: u32,
    ) -> Result<Vec<MenuItemInfo>, WindowError>;
    /// Pin the session to `app`: bring it forward and make every later
    /// perception/action call target it instead of whatever is frontmost.
    /// Passing `None` releases the pin.
    async fn focus_app(&self, app: Option<&str>) -> Result<Option<String>, WindowError>;
    /// Pin the session to `app` without bringing it forward or touching its
    /// windows: the same pin as [`WindowBackend::focus_app`], none of the
    /// activation. It is what an agent driving an application in the
    /// background needs, and what `foreground = "never"` allows.
    async fn pin_app(&self, app: &str) -> Result<TargetInfo, WindowError>;
    /// The application a call with this `app` would act on, and why that one.
    /// Reads only: it neither pins anything nor changes what is frontmost.
    /// A name that matches no running application is `NotFound`; with no name
    /// and nothing pinned it is the frontmost application, never a guess.
    async fn describe_target(&self, app: Option<&str>) -> Result<TargetInfo, WindowError>;
    /// The pinned application, if one is set and still running.
    async fn pinned(&self) -> Option<TargetInfo>;
    /// Start listening for changes in `app`'s UI (the frontmost app when
    /// `None`), so a wait can re-check when something happens instead of on a
    /// timer. `None` means this platform or this app gives no events and the
    /// wait polls. It must be passive: it neither pins nor raises the app.
    /// Everything it started is torn down when the [`ChangeWatch`] drops.
    async fn watch_changes(&self, _app: Option<&str>) -> Option<ChangeWatch> {
        None
    }
    fn platform(&self) -> &'static str;
}

/// A live subscription to "something changed in this app's UI".
///
/// `changed` has a permit stored whenever a notification arrived since it was
/// last awaited, so a change during an observation is never missed, and many
/// notifications in a burst are one wake-up. Dropping the watch drops `guard`,
/// which is where a backend puts the teardown of whatever it started (an
/// observer, its run-loop source and thread).
pub struct ChangeWatch {
    pub changed: std::sync::Arc<tokio::sync::Notify>,
    _guard: Box<dyn Send>,
}

impl ChangeWatch {
    pub fn new(changed: std::sync::Arc<tokio::sync::Notify>, guard: impl Send + 'static) -> Self {
        ChangeWatch {
            changed,
            _guard: Box::new(guard),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An unrecognised scope must be rejected, not quietly narrowed to `app`:
    /// a caller asking for a system sweep and silently getting one app's
    /// dialogs would conclude no authentication prompt is showing.
    #[test]
    fn dialog_scope_rejects_anything_it_does_not_understand() {
        assert_eq!(DialogScope::parse("app"), Some(DialogScope::App));
        assert_eq!(DialogScope::parse("system"), Some(DialogScope::System));
        assert_eq!(DialogScope::parse("System"), None);
        assert_eq!(DialogScope::parse("all"), None);
        assert_eq!(DialogScope::parse(""), None);
    }

    #[test]
    fn a_named_app_beats_a_pin_and_a_pin_beats_the_frontmost_app() {
        assert_eq!(target_source(Some("Finder"), true), TargetSource::Named);
        assert_eq!(target_source(Some("Finder"), false), TargetSource::Named);
        assert_eq!(target_source(None, true), TargetSource::Pinned);
        assert_eq!(target_source(None, false), TargetSource::Frontmost);
    }

    /// A blank name is not a request for an application. Treating it as one
    /// would resolve to nothing and error, or to the first process that
    /// matches the empty string.
    #[test]
    fn a_blank_name_is_no_name() {
        assert_eq!(target_source(Some(""), true), TargetSource::Pinned);
        assert_eq!(target_source(Some("  "), false), TargetSource::Frontmost);
    }

    #[test]
    fn the_window_index_error_names_the_app_and_how_many_windows_it_has() {
        let m = window_index_error("Finder", Some(412), 5, 2);
        assert!(m.contains("'Finder' (pid 412)"), "{m}");
        assert!(m.contains("2 windows"), "{m}");
        assert!(m.contains("window_id 5"), "{m}");
        let m = window_index_error("Finder", None, 0, 0);
        assert!(m.contains("no windows") && m.contains("'Finder'"), "{m}");
        assert!(window_index_error("Notes", Some(9), 3, 1).contains("1 window "));
    }

    #[test]
    fn a_window_id_past_the_end_is_an_error_not_the_last_window() {
        let w = |id| WindowInfo {
            id,
            app: Some("Notes".into()),
            pid: Some(9),
            title: None,
            bounds: None,
            minimized: false,
            state: WindowState::Onscreen,
        };
        let ws = [w(0), w(1)];
        assert_eq!(window_at(&ws, "Notes", Some(9), 1).unwrap().id, 1);
        let e = window_at(&ws, "Notes", Some(9), 2).unwrap_err();
        assert!(e.contains("2 windows"), "{e}");
        assert!(window_at(&[], "Notes", Some(9), 0).is_err());
    }

    #[test]
    fn window_action_parse_is_exact() {
        assert_eq!(WindowAction::parse("focus"), Some(WindowAction::Focus));
        assert_eq!(WindowAction::parse("Focus"), None);
    }
}
