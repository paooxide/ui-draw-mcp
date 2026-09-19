//! Windows, applications, menus and dialogs, read from AT-SPI and driven
//! through the application's own D-Bus activation and the GNOME shortcuts.
//!
//! Wayland keeps window management inside the compositor. There is no
//! client-side API to move, resize or stack another program's window, and
//! GNOME exposes none over D-Bus either. What a client can do is ask an
//! application to present itself (`org.freedesktop.Application.Activate`),
//! and press the desktop's own keyboard shortcuts. Both are real actions;
//! neither is a promise the compositor will oblige, so every one is verified
//! through the tree afterwards where a verification exists.

use std::time::Duration;

use async_trait::async_trait;
use atspi::{AccessibilityConnection, Interface, ObjectRefOwned, Role, State};
use mcp_input::{InputBackend, InputError};
use mcp_window::{
    DialogInfo, DialogScope, MenuItemInfo, Rect, WindowAction, WindowBackend, WindowError,
    WindowInfo,
};
use tokio::time::sleep;

use crate::a11y::{self, AppInfo};
use crate::backend::LinuxBackend;
use crate::launch;
use crate::roles;

fn to_rect(b: mcp_a11y::Bounds) -> Rect {
    Rect {
        x: b.x,
        y: b.y,
        w: b.w,
        h: b.h,
    }
}

fn fail(e: impl std::fmt::Display) -> WindowError {
    WindowError::Failed(e.to_string())
}

/// The honest, actionable message when GNOME will not foreground a window.
/// Shared by the window backend and the typing path so both say the same
/// thing.
pub fn cannot_foreground_msg(app: &str) -> String {
    format!(
        "GNOME declined to bring '{app}' to the front (focus-stealing prevention keeps a          background application from taking focus while another is active). You do not need          focus to act on it: ui_action presses its controls and set_value fills its fields          through the accessibility API, both working on a background window. To send          keystrokes, switch to '{app}' yourself or minimise the focused window first."
    )
}

fn input_to_window(e: InputError) -> WindowError {
    match e {
        InputError::PermissionDenied(m) => WindowError::PermissionDenied(m),
        InputError::NotFound(m) => WindowError::NotFound(m),
        InputError::Unsupported(m) => WindowError::Unsupported(m),
        InputError::InvalidArgs(m) => WindowError::Failed(m),
        InputError::Failed(m) => WindowError::Failed(m),
    }
}

/// Ask an application to present its window.
///
/// D-Bus activatable apps (every GNOME app, most GTK4 ones) answer to
/// `org.freedesktop.Application.Activate` on their own bus name, which is
/// also what the accessibility bus calls them. Anything else is launched
/// again through its desktop entry, which single-instance applications treat
/// as "raise the window I already have".
pub async fn activate_app(conn: &AccessibilityConnection, app: &AppInfo) -> Result<(), InputError> {
    let session = zbus::Connection::session()
        .await
        .map_err(|e| InputError::Failed(format!("session bus: {e}")))?;
    let name = app.name.trim();
    if name.contains('.') && !name.starts_with('.') && !name.ends_with('.') {
        if let Ok(bus) = zbus::names::BusName::try_from(name.to_string()) {
            let dbus = zbus::fdo::DBusProxy::new(&session)
                .await
                .map_err(|e| InputError::Failed(format!("dbus: {e}")))?;
            if dbus.name_has_owner(bus.clone()).await.unwrap_or(false) {
                let path = format!("/{}", name.replace('.', "/").replace('-', "_"));
                let proxy = zbus::Proxy::new(&session, bus, path, "org.freedesktop.Application")
                    .await
                    .map_err(|e| InputError::Failed(format!("application proxy: {e}")))?;
                let platform: std::collections::HashMap<String, zbus::zvariant::Value<'_>> =
                    std::collections::HashMap::new();
                match proxy.call::<_, _, ()>("Activate", &(platform,)).await {
                    Ok(()) => return Ok(()),
                    Err(e) => {
                        tracing::debug!(app = name, error = %e, "Activate over D-Bus failed; falling back to the desktop entry")
                    }
                }
            }
        }
    }
    let entries = launch::all_entries(&launch::application_dirs());
    let entry = entries
        .iter()
        .find(|e| launch::entry_matches_a11y_name(e, name))
        .or_else(|| launch::resolve(&entries, name));
    let Some(entry) = entry else {
        return Err(InputError::Unsupported(format!(
            "'{name}' is neither D-Bus activatable nor known by a desktop entry, so it cannot be raised; press its window with mouse_action instead"
        )));
    };
    let out = tokio::process::Command::new("/usr/bin/gtk-launch")
        .arg(&entry.id)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .map_err(|e| InputError::Failed(format!("gtk-launch: {e}")))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(InputError::Failed(format!(
            "gtk-launch {} failed: {}",
            entry.id,
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
    .map(|_| {
        let _ = conn;
    })
}

/// The desktop entries visible right now.
fn entries() -> Vec<launch::DesktopEntry> {
    launch::all_entries(&launch::application_dirs())
}

impl LinuxBackend {
    async fn refresh_windows(
        &self,
        app: Option<&AppInfo>,
    ) -> Result<Vec<a11y::WindowRef>, WindowError> {
        let conn = self.conn().await.map_err(WindowError::PermissionDenied)?;
        let ws = match app {
            Some(a) => a11y::windows_of(&conn, a).await.map_err(fail)?,
            None => a11y::all_windows(&conn).await.map_err(fail)?,
        };
        self.lock().windows = ws.clone();
        Ok(ws)
    }

    /// A window by the id `list_windows` reported, re-listing if needed.
    pub(crate) async fn window_by_id(&self, id: u32) -> Result<a11y::WindowRef, WindowError> {
        let cached = self.lock().windows.get(id as usize).cloned();
        if let Some(w) = cached {
            return Ok(w);
        }
        let ws = self.refresh_windows(None).await?;
        ws.get(id as usize).cloned().ok_or_else(|| {
            WindowError::NotFound(format!("no window with id {id}; call list_windows first"))
        })
    }

    async fn pick_window(
        &self,
        app: &AppInfo,
        title: Option<&str>,
    ) -> Result<a11y::WindowRef, WindowError> {
        let ws = self.refresh_windows(Some(app)).await?;
        if ws.is_empty() {
            return Err(WindowError::NotFound(format!(
                "'{}' has no windows",
                app.name
            )));
        }
        match title {
            Some(t) => {
                let tl = t.to_lowercase();
                ws.into_iter()
                    .find(|w| w.title.to_lowercase().contains(&tl))
                    .ok_or_else(|| {
                        WindowError::NotFound(format!("no window titled '{t}' in '{}'", app.name))
                    })
            }
            None => Ok(ws
                .iter()
                .find(|w| w.states.contains(State::Active))
                .cloned()
                .unwrap_or_else(|| ws[0].clone())),
        }
    }

    async fn wait_active(&self, app: &AppInfo, want: bool, tries: u32) -> bool {
        for _ in 0..tries {
            if let Ok(ws) = self.refresh_windows(Some(app)).await {
                let active = ws.iter().any(|w| w.states.contains(State::Active));
                if active == want {
                    return true;
                }
            }
            sleep(Duration::from_millis(100)).await;
        }
        false
    }

    /// Bring an application's window to the front and confirm it took.
    ///
    /// `Application.Activate` (via [`activate_app`]) is the only lever a client
    /// has on GNOME, and it also unminimizes. It works in the common case
    /// where nothing else is holding focus, which is what an agent driving the
    /// desktop usually faces. When another application is actively focused,
    /// GNOME's focus-stealing prevention accepts the request but leaves the
    /// window in the background, so this confirms the window actually became
    /// active and reports honestly, pointing at the focus-independent paths,
    /// when it did not.
    async fn bring_to_front(
        &self,
        conn: &AccessibilityConnection,
        target: &AppInfo,
    ) -> Result<(), WindowError> {
        // Already active: nothing to do, and no need to poll.
        if let Ok(ws) = a11y::windows_of(conn, target).await {
            if ws.iter().any(|w| w.states.contains(State::Active)) {
                return Ok(());
            }
        }
        activate_app(conn, target).await.map_err(input_to_window)?;
        if self.wait_active(target, true, 20).await {
            Ok(())
        } else {
            Err(WindowError::Failed(cannot_foreground_msg(&target.name)))
        }
    }

    /// Resolve `app` for a window operation without pinning it.
    async fn app_for(&self, app: Option<&str>) -> Result<AppInfo, WindowError> {
        self.resolve_target(app)
            .await
            .map_err(WindowError::NotFound)
    }
}

/// Find the menu bar under an application, then walk a title path.
async fn menu_container(
    conn: &AccessibilityConnection,
    app: &AppInfo,
    path: &[String],
) -> Result<ObjectRefOwned, WindowError> {
    let nodes = a11y::descendants(conn, &app.obj, 4, 600).await;
    let bar = nodes
        .iter()
        .find(|(_, role, _, _)| *role == Role::MenuBar)
        .map(|(o, _, _, _)| o.clone())
        .ok_or_else(|| {
            WindowError::NotFound(format!(
                "'{}' has no menu bar; GNOME apps keep their menu behind a button (look for a 'Main Menu' or 'Menu' button in the tree)",
                app.name
            ))
        })?;
    let mut cur = bar;
    for title in path {
        let kids = a11y::descendants(conn, &cur, 1, 200).await;
        let tl = title
            .trim_end_matches('\u{2026}')
            .trim_end_matches("...")
            .to_lowercase();
        let next = kids
            .iter()
            .skip(1)
            .find(|(_, role, name, _)| {
                matches!(
                    role,
                    Role::Menu | Role::MenuItem | Role::CheckMenuItem | Role::RadioMenuItem
                ) && name
                    .trim_end_matches('\u{2026}')
                    .trim_end_matches("...")
                    .to_lowercase()
                    == tl
            })
            .map(|(o, _, _, _)| o.clone())
            .ok_or_else(|| WindowError::NotFound(format!("no menu item '{title}'")))?;
        cur = next;
    }
    Ok(cur)
}

async fn press(conn: &AccessibilityConnection, obj: &ObjectRefOwned) -> Result<(), WindowError> {
    use atspi::proxy::proxy_ext::ProxyExt;
    let p = a11y::proxy_for(conn, obj).await.map_err(fail)?;
    let ifaces = p.get_interfaces().await.map_err(fail)?;
    if !ifaces.contains(Interface::Action) {
        return Err(WindowError::Unsupported("item has no actions".into()));
    }
    let a = p
        .proxies()
        .await
        .map_err(fail)?
        .action()
        .await
        .map_err(fail)?;
    let actions = a.get_actions().await.map_err(fail)?;
    let idx = actions
        .iter()
        .position(|x| a11y::PRESS_ACTIONS.contains(&x.name.to_lowercase().as_str()))
        .unwrap_or(0);
    if a.do_action(idx as i32).await.map_err(fail)? {
        Ok(())
    } else {
        Err(WindowError::Failed("the item declined its action".into()))
    }
}

/// Convert an AT-SPI key binding string (`<Control>s`, `<Primary><Shift>n`)
/// into `keyboard_shortcut` syntax.
pub fn shortcut_from_binding(binding: &str) -> Option<String> {
    let b = binding.split(';').next()?.trim();
    if b.is_empty() {
        return None;
    }
    let mut mods = Vec::new();
    let mut rest = b;
    while let Some(start) = rest.find('<') {
        let end = rest[start..].find('>')? + start;
        let m = rest[start + 1..end].to_lowercase();
        mods.push(match m.as_str() {
            "control" | "ctrl" | "primary" => "ctrl",
            "shift" => "shift",
            "alt" | "mod1" => "alt",
            "super" | "mod4" | "meta" => "super",
            _ => return None,
        });
        rest = &rest[end + 1..];
    }
    let key = rest.trim().to_lowercase();
    if key.is_empty() {
        return None;
    }
    let key = match key.as_str() {
        "return" | "kp_enter" => "return".to_string(),
        "escape" => "escape".to_string(),
        "backspace" => "delete".to_string(),
        "page_up" => "pageup".to_string(),
        "page_down" => "pagedown".to_string(),
        k => k.to_string(),
    };
    mods.push(&key);
    Some(mods.join("+"))
}

#[async_trait]
impl WindowBackend for LinuxBackend {
    async fn list_windows(&self, app_name: Option<&str>) -> Result<Vec<WindowInfo>, WindowError> {
        // With no app named and no pinned target, answer about the whole
        // desktop, so "what is open?" is not silently narrowed.
        let app = match app_name {
            Some(_) => Some(self.app_for(app_name).await?),
            None => self.pinned_target().await,
        };
        let ws = self.refresh_windows(app.as_ref()).await?;
        Ok(ws
            .iter()
            .enumerate()
            .map(|(i, w)| WindowInfo {
                id: i as u32,
                app: Some(w.app_name.clone()),
                title: Some(w.title.clone()).filter(|t| !t.is_empty()),
                bounds: w.bounds.map(to_rect),
                minimized: w.states.contains(State::Iconified),
            })
            .collect())
    }

    /// Applications with windows, named as a person would.
    async fn list_apps(&self) -> Result<Vec<String>, WindowError> {
        let conn = self.conn().await.map_err(WindowError::PermissionDenied)?;
        let apps = a11y::applications(&conn).await.map_err(fail)?;
        let entries = entries();
        let mut out = Vec::new();
        for a in apps
            .iter()
            .filter(|a| !a11y::is_system_ui(&a.name) && !a.name.is_empty())
        {
            let has_window = a11y::windows_of(&conn, a)
                .await
                .map(|w| !w.is_empty())
                .unwrap_or(false);
            if !has_window {
                continue;
            }
            let friendly = entries
                .iter()
                .find(|e| launch::entry_matches_a11y_name(e, &a.name))
                .map(|e| e.name.clone())
                .unwrap_or_else(|| a.name.clone());
            out.push(friendly);
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    async fn launch(&self, app: &str) -> Result<(), WindowError> {
        let entries = entries();
        let entry = launch::resolve(&entries, app).ok_or_else(|| {
            WindowError::NotFound(format!(
                "no application called '{app}' (no desktop entry matches)"
            ))
        })?;
        let out = tokio::process::Command::new("/usr/bin/gtk-launch")
            .arg(&entry.id)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .map_err(|e| WindowError::Failed(format!("gtk-launch: {e}")))?;
        if !out.status.success() {
            return Err(WindowError::Failed(format!(
                "could not launch '{}': {}",
                entry.name,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        // Wait for it to show up on the accessibility bus, so the next call
        // can drive it; a launch that returns before that is a race.
        let conn = self.conn().await.map_err(WindowError::PermissionDenied)?;
        for _ in 0..50 {
            let apps = a11y::applications(&conn).await.unwrap_or_default();
            if let Some(a) = apps
                .iter()
                .find(|a| launch::entry_matches_a11y_name(entry, &a.name))
            {
                if a11y::windows_of(&conn, a)
                    .await
                    .map(|w| !w.is_empty())
                    .unwrap_or(false)
                {
                    self.set_target(a);
                    return Ok(());
                }
            }
            sleep(Duration::from_millis(100)).await;
        }
        tracing::warn!(
            app = entry.id,
            "launched, but it has not appeared on the accessibility bus after 5s"
        );
        Ok(())
    }

    async fn close_app(&self, target: &str) -> Result<(), WindowError> {
        let conn = self.conn().await.map_err(WindowError::PermissionDenied)?;
        let app = a11y::app_by_name(&conn, target)
            .await
            .map_err(fail)?
            .ok_or_else(|| WindowError::NotFound(format!("no running app matching '{target}'")))?;
        let pid = a11y::pid_of(&conn, &app.obj).await.ok_or_else(|| {
            WindowError::Failed(format!("cannot find the process behind '{}'", app.name))
        })?;
        if pid <= 1 || pid == std::process::id() {
            return Err(WindowError::Failed(format!("refusing to signal pid {pid}")));
        }
        let out = tokio::process::Command::new("/usr/bin/kill")
            .args(["-TERM", &pid.to_string()])
            .output()
            .await
            .map_err(|e| WindowError::Failed(format!("kill: {e}")))?;
        if !out.status.success() {
            return Err(WindowError::Failed(format!(
                "could not terminate '{}' (pid {pid}): {}",
                app.name,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        if self
            .lock()
            .target
            .as_ref()
            .is_some_and(|t| t.obj == app.obj)
        {
            self.clear_target();
        }
        Ok(())
    }

    async fn control_window(
        &self,
        app: Option<&str>,
        title: Option<&str>,
        action: WindowAction,
        bounds: Option<Rect>,
    ) -> Result<(), WindowError> {
        let target = self.app_for(app).await?;
        let win = self.pick_window(&target, title).await?;
        let conn = self.conn().await.map_err(WindowError::PermissionDenied)?;
        match action {
            WindowAction::Focus => self.bring_to_front(&conn, &target).await,
            WindowAction::Close => {
                self.bring_to_front(&conn, &target).await?;
                self.key_combo("alt+f4").await.map_err(input_to_window)?;
                for _ in 0..20 {
                    sleep(Duration::from_millis(100)).await;
                    let ws = self
                        .refresh_windows(Some(&target))
                        .await
                        .unwrap_or_default();
                    if !ws.iter().any(|w| w.obj == win.obj) {
                        return Ok(());
                    }
                }
                Err(WindowError::Failed(
                    "the window is still open after Alt+F4; it may be asking to save (see handle_dialogs)".into(),
                ))
            }
            WindowAction::Minimize => {
                self.bring_to_front(&conn, &target).await?;
                self.key_combo("super+h").await.map_err(input_to_window)?;
                if self.wait_active(&target, false, 20).await {
                    Ok(())
                } else {
                    Err(WindowError::Failed(
                        "the window is still active after Super+H (GNOME's minimize shortcut)"
                            .into(),
                    ))
                }
            }
            WindowAction::Maximize => {
                self.bring_to_front(&conn, &target).await?;
                self.key_combo("super+up").await.map_err(input_to_window)
            }
            WindowAction::Restore => {
                // Unminimize (and raise) is what Activate does, and GNOME
                // honours it for a minimized window; then unmaximize if it was
                // also maximized.
                if win.states.contains(State::Iconified) {
                    return self.bring_to_front(&conn, &target).await;
                }
                self.bring_to_front(&conn, &target).await?;
                self.key_combo("super+down").await.map_err(input_to_window)
            }
            WindowAction::Move | WindowAction::Resize => {
                let _ = bounds;
                Err(WindowError::Unsupported(
                    "Wayland gives clients no way to move or resize another program's window; use keyboard_shortcut with the desktop's tiling keys (super+left, super+right) instead".into(),
                ))
            }
        }
    }

    async fn menu_open(&self, app: Option<&str>, path: &[String]) -> Result<(), WindowError> {
        let target = self.app_for(app).await?;
        let conn = self.conn().await.map_err(WindowError::PermissionDenied)?;
        if path.is_empty() {
            return Err(WindowError::Failed("menu_open needs a path".into()));
        }
        // Open each level in turn so submenus exist before they are walked.
        for depth in 1..=path.len() {
            let item = menu_container(&conn, &target, &path[..depth]).await?;
            press(&conn, &item).await?;
            sleep(Duration::from_millis(80)).await;
        }
        Ok(())
    }

    async fn menu_invoke(&self, app: Option<&str>, path: &[String]) -> Result<(), WindowError> {
        if path.len() < 2 {
            return Err(WindowError::Failed(
                "menu_invoke needs a menu and an item".into(),
            ));
        }
        let target = self.app_for(app).await?;
        let conn = self.conn().await.map_err(WindowError::PermissionDenied)?;
        for depth in 1..path.len() {
            let item = menu_container(&conn, &target, &path[..depth]).await?;
            press(&conn, &item).await?;
            sleep(Duration::from_millis(80)).await;
        }
        let leaf = menu_container(&conn, &target, path).await?;
        press(&conn, &leaf).await
    }

    async fn list_dialogs(
        &self,
        app: Option<&str>,
        scope: DialogScope,
    ) -> Result<Vec<DialogInfo>, WindowError> {
        let conn = self.conn().await.map_err(WindowError::PermissionDenied)?;
        let apps: Vec<AppInfo> = match scope {
            DialogScope::App => vec![self.app_for(app).await?],
            DialogScope::System => a11y::applications(&conn).await.map_err(fail)?,
        };
        let mut out = Vec::new();
        for a in apps {
            let Ok(p) = a11y::proxy_for(&conn, &a.obj).await else {
                continue;
            };
            let Ok(children) = p.get_children().await else {
                continue;
            };
            for c in children {
                let Ok(cp) = a11y::proxy_for(&conn, &c).await else {
                    continue;
                };
                let (Ok(role), Ok(states)) = (cp.get_role().await, cp.get_state().await) else {
                    continue;
                };
                if !roles::is_dialog(role, states) || !states.contains(State::Showing) {
                    continue;
                }
                let title = cp.name().await.unwrap_or_default();
                let inner = a11y::descendants(&conn, &c, 8, 400).await;
                let mut buttons = Vec::new();
                let mut text = Vec::new();
                let mut default_button = None;
                let mut has_secure_field = false;
                for (_, r, n, s) in inner.iter().skip(1) {
                    match r {
                        Role::Button | Role::ToggleButton => {
                            if !n.is_empty() {
                                if s.contains(State::IsDefault) {
                                    default_button = Some(n.clone());
                                }
                                buttons.push(n.clone());
                            }
                        }
                        Role::Label | Role::Static | Role::Heading | Role::Paragraph => {
                            if !n.is_empty() {
                                text.push(n.clone());
                            }
                        }
                        Role::PasswordText => has_secure_field = true,
                        _ => {}
                    }
                }
                let cancel_button = buttons
                    .iter()
                    .find(|b| {
                        let l = b.to_lowercase();
                        l.starts_with("cancel") || l == "close" || l == "dismiss" || l == "no"
                    })
                    .cloned();
                out.push(DialogInfo {
                    id: 0,
                    app: Some(a.name.clone()),
                    title: Some(title).filter(|t| !t.is_empty()),
                    kind: roles::dialog_kind(role).to_string(),
                    bounds: a11y::extents(&cp).await.map(to_rect),
                    buttons,
                    text,
                    default_button,
                    cancel_button,
                    has_secure_field,
                });
            }
        }
        for (i, d) in out.iter_mut().enumerate() {
            d.id = i as u32;
        }
        Ok(out)
    }

    async fn menu_list(
        &self,
        app: Option<&str>,
        path: &[String],
        depth: u32,
    ) -> Result<Vec<MenuItemInfo>, WindowError> {
        use atspi::proxy::proxy_ext::ProxyExt;
        let target = self.app_for(app).await?;
        let conn = self.conn().await.map_err(WindowError::PermissionDenied)?;
        let container = menu_container(&conn, &target, path).await?;
        let depth = depth.clamp(1, 5) as usize;
        let nodes = a11y::descendants(&conn, &container, depth, 500).await;
        let mut out = Vec::new();
        // Rebuild each item's title path from its ancestry by walking parents.
        for (obj, role, name, states) in nodes.iter().skip(1) {
            if !matches!(
                role,
                Role::Menu
                    | Role::MenuItem
                    | Role::CheckMenuItem
                    | Role::RadioMenuItem
                    | Role::TearoffMenuItem
            ) {
                continue;
            }
            if name.is_empty() {
                continue;
            }
            let mut ancestry = Vec::new();
            let mut cur = obj.clone();
            for _ in 0..depth {
                let Ok(p) = a11y::proxy_for(&conn, &cur).await else {
                    break;
                };
                let Ok(parent) = p.parent().await else { break };
                if parent.is_null() || parent == container {
                    break;
                }
                if let Ok(pp) = a11y::proxy_for(&conn, &parent).await {
                    if let Ok(pn) = pp.name().await {
                        if let Ok(pr) = pp.get_role().await {
                            if matches!(pr, Role::Menu | Role::MenuItem) && !pn.is_empty() {
                                ancestry.push(pn);
                            }
                        }
                    }
                }
                cur = parent;
            }
            ancestry.reverse();
            let mut full: Vec<String> = path.to_vec();
            full.extend(ancestry);
            full.push(name.clone());
            let shortcut = match a11y::proxy_for(&conn, obj).await {
                Ok(p) => match p.proxies().await {
                    Ok(px) => match px.action().await {
                        Ok(a) => a
                            .get_key_binding(0)
                            .await
                            .ok()
                            .and_then(|b| shortcut_from_binding(&b)),
                        Err(_) => None,
                    },
                    Err(_) => None,
                },
                Err(_) => None,
            };
            out.push(MenuItemInfo {
                path: full,
                title: name.clone(),
                enabled: states.contains(State::Sensitive) || states.contains(State::Enabled),
                has_submenu: *role == Role::Menu,
                shortcut,
            });
        }
        Ok(out)
    }

    async fn focus_app(&self, app: Option<&str>) -> Result<Option<String>, WindowError> {
        let Some(name) = app.map(str::trim).filter(|s| !s.is_empty()) else {
            self.clear_target();
            return Ok(None);
        };
        let target = self.app_for(Some(name)).await?;
        let conn = self.conn().await.map_err(WindowError::PermissionDenied)?;
        let already = self
            .refresh_windows(Some(&target))
            .await?
            .iter()
            .any(|w| w.states.contains(State::Active));
        if !already {
            activate_app(&conn, &target)
                .await
                .map_err(input_to_window)?;
            if !self.wait_active(&target, true, 30).await {
                return Err(WindowError::Failed(format!(
                    "'{}' is pinned as the target but could not be brought to the front",
                    target.name
                )));
            }
        }
        Ok(Some(target.name))
    }

    fn platform(&self) -> &'static str {
        "linux"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_foreground_refusal_names_the_app_and_the_focus_free_paths() {
        let m = cannot_foreground_msg("org.gnome.TextEditor");
        assert!(m.contains("org.gnome.TextEditor"));
        assert!(m.contains("ui_action") && m.contains("set_value"));
        assert!(m.to_lowercase().contains("focus-stealing"));
    }

    #[test]
    fn at_spi_key_bindings_become_shortcut_syntax() {
        assert_eq!(
            shortcut_from_binding("<Control>s").as_deref(),
            Some("ctrl+s")
        );
        assert_eq!(
            shortcut_from_binding("<Primary><Shift>n").as_deref(),
            Some("ctrl+shift+n")
        );
        assert_eq!(shortcut_from_binding("<Alt>F4").as_deref(), Some("alt+f4"));
        assert_eq!(
            shortcut_from_binding("<Super>Up").as_deref(),
            Some("super+up")
        );
        assert_eq!(
            shortcut_from_binding("<Control>q;<Control>w").as_deref(),
            Some("ctrl+q")
        );
        assert_eq!(shortcut_from_binding("Return").as_deref(), Some("return"));
        assert_eq!(shortcut_from_binding(""), None);
        assert_eq!(shortcut_from_binding("<Hyper>x"), None);
        assert_eq!(shortcut_from_binding("<Control>"), None);
        assert_eq!(shortcut_from_binding("<Control"), None);
    }
}
