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

/// A window, as observed by the backend.
#[derive(Debug, Clone, Serialize)]
pub struct WindowInfo {
    /// Backend-local id (index within the app's window list).
    pub id: u32,
    pub app: Option<String>,
    pub title: Option<String>,
    pub bounds: Option<Rect>,
    pub minimized: bool,
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
}

/// Platform window/app/menu backend.
#[async_trait]
pub trait WindowBackend: Send + Sync {
    /// Windows of an app (or the focused app when `app` is `None`).
    async fn list_windows(&self, app: Option<&str>) -> Result<Vec<WindowInfo>, WindowError>;
    /// Names of running apps.
    async fn list_apps(&self) -> Result<Vec<String>, WindowError>;
    async fn launch(&self, app: &str) -> Result<(), WindowError>;
    async fn close_app(&self, target: &str) -> Result<(), WindowError>;
    async fn control_window(
        &self,
        app: Option<&str>,
        title: Option<&str>,
        action: WindowAction,
        bounds: Option<Rect>,
    ) -> Result<(), WindowError>;
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
    fn platform(&self) -> &'static str;
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
    fn window_action_parse_is_exact() {
        assert_eq!(WindowAction::parse("focus"), Some(WindowAction::Focus));
        assert_eq!(WindowAction::parse("Focus"), None);
    }
}
