//! Reading the AT-SPI2 tree.
//!
//! Every node costs a handful of D-Bus round trips, so the walk is breadth
//! first with the calls for one level issued together, bounded by a node cap
//! and a wall-clock budget, and it reports `partial` when either bound cut it
//! short. A tree that is a prefix of the real one has to be distinguishable
//! from a tree that really is that small, or the agent concludes the button
//! it needs does not exist.
//!
//! Unlike macOS, an AT-SPI object reference (bus name plus object path) is a
//! stable handle for as long as the widget lives, so a snapshot ref keeps the
//! object itself and only falls back to the child-index path, then to an
//! identity search, when the object has gone defunct.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use atspi::proxy::accessible::{AccessibleProxy, ObjectRefExt};
use atspi::proxy::proxy_ext::ProxyExt;
use atspi::{AccessibilityConnection, CoordType, Interface, ObjectRefOwned, Role, State, StateSet};
use mcp_a11y::{is_interactive_role, Bounds, UiNode};
use tokio::sync::Semaphore;

use crate::roles;

/// Wall-clock bound on a walk. The same bound the macOS backend uses, for
/// the same reason: `wait_for` polls this, and a poll that overruns its
/// deadline is a wait that lies about its timeout.
pub const WALK_BUDGET: Duration = Duration::from_secs(3);
/// Node cap per snapshot.
pub const MAX_NODES: usize = 4000;
/// Children read per node. A table with ten thousand rows is a table whose
/// first few hundred rows are what fits in a context window anyway.
const MAX_CHILDREN: usize = 400;
/// Characters of a text value to read. Terminal tails are kept, heads for
/// everything else, matching the flattener's expectations.
const MAX_TEXT: i32 = 4000;
/// Concurrent node fetches in flight. Enough to hide latency, few enough
/// not to flood an application's main loop.
const CONCURRENCY: usize = 24;

/// How a snapshot ref finds its widget again.
#[derive(Debug, Clone)]
pub struct NodeRef {
    pub obj: ObjectRefOwned,
    pub app: ObjectRefOwned,
    pub path: Vec<usize>,
    pub role: String,
    pub name: Option<String>,
}

/// An application on the accessibility bus.
#[derive(Debug, Clone)]
pub struct AppInfo {
    pub obj: ObjectRefOwned,
    pub name: String,
}

/// Processes that draw the desktop itself. None is an application anyone
/// means to drive, and `gnome-shell` is where every popup and notification
/// lives, so it stays out of "what is open" but in a system dialog sweep.
pub const SYSTEM_UI_APPS: &[&str] = &[
    "gnome-shell",
    "xdg-desktop-portal-gtk",
    "xdg-desktop-portal-gnome",
    "xdg-desktop-portal-kde",
    "ibus-extension-gtk3",
    "ibus-x11",
    "evolution-alarm-notify",
    "gsd-media-keys",
    "gsd-xsettings",
    "at-spi-bus-launcher",
    "mutter-x11-frames",
    "plasmashell",
    "kwin_wayland",
];

pub fn is_system_ui(name: &str) -> bool {
    SYSTEM_UI_APPS.iter().any(|s| s.eq_ignore_ascii_case(name))
}

/// Open the accessibility bus. Fails with a message that names the fix when
/// there is no bus (no session, or AT-SPI not installed).
pub async fn connect() -> Result<AccessibilityConnection, String> {
    let conn = tokio::time::timeout(Duration::from_secs(5), AccessibilityConnection::new())
        .await
        .map_err(|_| "timed out reaching the accessibility bus (org.a11y.Bus)".to_string())?
        .map_err(|e| format!("cannot reach the accessibility bus: {e}"))?;
    // GTK3 and some Electron apps only export a tree while the session says
    // accessibility is on. Screen readers flip this switch on start; so do we.
    // Best effort: a failure here is logged, not fatal, because GTK4 apps
    // export regardless.
    match atspi::connection::read_session_accessibility().await {
        Ok(true) => {}
        Ok(false) => match atspi::connection::set_session_accessibility(true).await {
            Ok(()) => tracing::info!("enabled session accessibility (org.a11y.Status IsEnabled)"),
            Err(e) => {
                tracing::warn!(error = %e, "could not enable session accessibility; GTK3 and Electron apps may export no tree")
            }
        },
        Err(e) => tracing::debug!(error = %e, "could not read session accessibility status"),
    }
    Ok(conn)
}

pub async fn proxy_for<'c>(
    conn: &'c AccessibilityConnection,
    obj: &ObjectRefOwned,
) -> Result<AccessibleProxy<'c>, String> {
    obj.clone()
        .into_accessible_proxy(conn.connection())
        .await
        .map_err(|e| format!("accessible proxy: {e}"))
}

/// Applications currently on the bus, in registry order.
pub async fn applications(conn: &AccessibilityConnection) -> Result<Vec<AppInfo>, String> {
    let root = conn
        .root_accessible_on_registry()
        .await
        .map_err(|e| format!("accessibility registry root: {e}"))?;
    let children = root
        .get_children()
        .await
        .map_err(|e| format!("listing applications: {e}"))?;
    let mut out = Vec::with_capacity(children.len());
    for obj in children {
        if obj.is_null() {
            continue;
        }
        let name = match proxy_for(conn, &obj).await {
            Ok(p) => p.name().await.unwrap_or_default(),
            Err(_) => continue,
        };
        out.push(AppInfo { obj, name });
    }
    Ok(out)
}

/// The process behind an application, from the bus daemon.
pub async fn pid_of(conn: &AccessibilityConnection, obj: &ObjectRefOwned) -> Option<u32> {
    let name = obj.name()?.clone();
    let dbus = zbus::fdo::DBusProxy::new(conn.connection()).await.ok()?;
    dbus.get_connection_unix_process_id(name.into()).await.ok()
}

/// A window of an application, with what `list_windows` needs.
#[derive(Debug, Clone)]
pub struct WindowRef {
    pub obj: ObjectRefOwned,
    pub app_name: String,
    pub title: String,
    pub states: StateSet,
    pub bounds: Option<Bounds>,
}

/// Top-level windows of one application.
pub async fn windows_of(
    conn: &AccessibilityConnection,
    app: &AppInfo,
) -> Result<Vec<WindowRef>, String> {
    let proxy = proxy_for(conn, &app.obj).await?;
    let children = proxy
        .get_children()
        .await
        .map_err(|e| format!("windows of '{}': {e}", app.name))?;
    let mut out = Vec::new();
    for obj in children.into_iter().take(MAX_CHILDREN) {
        let Ok(p) = proxy_for(conn, &obj).await else {
            continue;
        };
        let (Ok(role), Ok(states)) = (p.get_role().await, p.get_state().await) else {
            continue;
        };
        if !roles::is_window(role) {
            continue;
        }
        let title = p.name().await.unwrap_or_default();
        let bounds = extents(&p).await;
        out.push(WindowRef {
            obj,
            app_name: app.name.clone(),
            title,
            states,
            bounds,
        });
    }
    Ok(out)
}

/// Every window on the bus, active one first, system UI excluded.
pub async fn all_windows(conn: &AccessibilityConnection) -> Result<Vec<WindowRef>, String> {
    let apps = applications(conn).await?;
    let mut out = Vec::new();
    for app in apps.iter().filter(|a| !is_system_ui(&a.name)) {
        if let Ok(ws) = windows_of(conn, app).await {
            out.extend(ws);
        }
    }
    out.sort_by_key(|w| !w.states.contains(State::Active));
    Ok(out)
}

/// The application whose window is active, if any.
pub async fn active_app(conn: &AccessibilityConnection) -> Result<Option<AppInfo>, String> {
    let apps = applications(conn).await?;
    for app in apps.iter().filter(|a| !is_system_ui(&a.name)) {
        if let Ok(ws) = windows_of(conn, app).await {
            if ws.iter().any(|w| w.states.contains(State::Active)) {
                return Ok(Some(app.clone()));
            }
        }
    }
    Ok(None)
}

/// Find an application by the name an agent would use: the a11y name, a
/// desktop-entry name or id, case-insensitive.
pub async fn app_by_name(
    conn: &AccessibilityConnection,
    query: &str,
) -> Result<Option<AppInfo>, String> {
    let apps = applications(conn).await?;
    let q = query.trim();
    if let Some(a) = apps.iter().find(|a| a.name.eq_ignore_ascii_case(q)) {
        return Ok(Some(a.clone()));
    }
    let entries = crate::launch::all_entries(&crate::launch::application_dirs());
    if let Some(entry) = crate::launch::resolve(&entries, q) {
        if let Some(a) = apps
            .iter()
            .find(|a| crate::launch::entry_matches_a11y_name(entry, &a.name))
        {
            return Ok(Some(a.clone()));
        }
    }
    let ql = q.to_lowercase();
    Ok(apps
        .iter()
        .find(|a| !is_system_ui(&a.name) && a.name.to_lowercase().contains(&ql))
        .cloned())
}

pub async fn extents(p: &AccessibleProxy<'_>) -> Option<Bounds> {
    let comp = p.proxies().await.ok()?.component().await.ok()?;
    let (x, y, w, h) = comp.get_extents(CoordType::Screen).await.ok()?;
    if w <= 0 || h <= 0 {
        return None;
    }
    Some(Bounds {
        x: x as f64,
        y: y as f64,
        w: w as f64,
        h: h as f64,
    })
}

/// What one fetch of a node yields.
struct Fetched {
    role: Role,
    name: String,
    description: String,
    states: StateSet,
    interfaces: atspi::InterfaceSet,
    children: Vec<ObjectRefOwned>,
    bounds: Option<Bounds>,
    value: Option<String>,
}

async fn fetch(conn: &AccessibilityConnection, obj: &ObjectRefOwned) -> Result<Fetched, String> {
    let p = proxy_for(conn, obj).await?;
    let (role, name, states, interfaces, children) = futures_util::try_join!(
        p.get_role(),
        p.name(),
        p.get_state(),
        p.get_interfaces(),
        p.get_children(),
    )
    .map_err(|e| format!("node: {e}"))?;
    let description = if name.is_empty() {
        p.description().await.unwrap_or_default()
    } else {
        String::new()
    };
    let (norm, _) = roles::normalize(role, states);
    let wants_bounds = is_interactive_role(&norm) || roles::is_window(role);
    let bounds = if wants_bounds && interfaces.contains(Interface::Component) {
        extents(&p).await
    } else {
        None
    };
    let value = if role == Role::PasswordText {
        None
    } else if roles::has_text_value(role) && interfaces.contains(Interface::Text) {
        text_value(&p, role == Role::Terminal).await
    } else if roles::has_numeric_value(role) && interfaces.contains(Interface::Value) {
        numeric_value(&p).await
    } else {
        None
    };
    Ok(Fetched {
        role,
        name,
        description,
        states,
        interfaces,
        children,
        bounds,
        value,
    })
}

async fn text_value(p: &AccessibleProxy<'_>, tail: bool) -> Option<String> {
    let t = p.proxies().await.ok()?.text().await.ok()?;
    let count = t.character_count().await.ok()?;
    if count <= 0 {
        return None;
    }
    let (start, end) = if tail {
        ((count - MAX_TEXT).max(0), count)
    } else {
        (0, count.min(MAX_TEXT))
    };
    let s = t.get_text(start, end).await.ok()?;
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

pub(crate) async fn numeric_value(p: &AccessibleProxy<'_>) -> Option<String> {
    let v = p.proxies().await.ok()?.value().await.ok()?;
    let cur = v.current_value().await.ok()?;
    if let Ok(text) = v.text().await {
        if !text.is_empty() {
            return Some(text);
        }
    }
    Some(if cur.fract() == 0.0 {
        format!("{}", cur as i64)
    } else {
        format!("{cur:.2}")
    })
}

/// The outcome of a walk.
pub struct Walked {
    pub root: UiNode,
    pub paths: HashMap<u64, NodeRef>,
    pub partial: bool,
    pub count: usize,
}

/// Walk from `root` (an application or any node) into a `UiNode` tree.
pub async fn walk(
    conn: Arc<AccessibilityConnection>,
    app: ObjectRefOwned,
    root: ObjectRefOwned,
    max_depth: usize,
    budget: Duration,
) -> Walked {
    struct Flat {
        node: UiNode,
        parent: Option<usize>,
    }
    let started = Instant::now();
    let sem = Arc::new(Semaphore::new(CONCURRENCY));
    let mut flat: Vec<Flat> = Vec::new();
    let mut paths: HashMap<u64, NodeRef> = HashMap::new();
    let mut next_id: u64 = 1;
    let mut partial = false;
    // (parent flat index, object, path from root, depth)
    let mut frontier: Vec<(Option<usize>, ObjectRefOwned, Vec<usize>, usize)> =
        vec![(None, root, Vec::new(), 0)];

    while !frontier.is_empty() {
        if started.elapsed() >= budget || flat.len() >= MAX_NODES {
            partial = true;
            break;
        }
        let level = std::mem::take(&mut frontier);
        let fetches = level.iter().map(|(_, obj, _, _)| {
            let conn = conn.clone();
            let sem = sem.clone();
            let obj = obj.clone();
            async move {
                let _permit = sem.acquire().await;
                tokio::time::timeout(Duration::from_secs(2), fetch(&conn, &obj))
                    .await
                    .unwrap_or_else(|_| Err("node fetch timed out".into()))
            }
        });
        let results = futures_util::future::join_all(fetches).await;
        for ((parent, obj, path, depth), result) in level.into_iter().zip(results) {
            let f = match result {
                Ok(f) => f,
                Err(e) => {
                    tracing::debug!(error = %e, "skipping unreadable node");
                    continue;
                }
            };
            if flat.len() >= MAX_NODES {
                partial = true;
                break;
            }
            let (role, secure) = roles::normalize(f.role, f.states);
            let name = if !f.name.is_empty() {
                Some(f.name.clone())
            } else if !f.description.is_empty() {
                Some(f.description.clone())
            } else {
                None
            };
            let id = next_id;
            next_id += 1;
            paths.insert(
                id,
                NodeRef {
                    obj: obj.clone(),
                    app: app.clone(),
                    path: path.clone(),
                    role: role.clone(),
                    name: name.clone(),
                },
            );
            let checked = if roles::is_checkable(f.role, f.states) {
                Some(f.states.contains(State::Checked))
            } else {
                None
            };
            let expanded = if f.states.contains(State::Expandable) {
                Some(f.states.contains(State::Expanded))
            } else {
                None
            };
            let node = UiNode {
                role,
                name,
                value: f.value,
                subrole: roles::subrole(f.role),
                secure,
                focused: f.states.contains(State::Focused),
                disabled: !f.states.contains(State::Sensitive)
                    && !f.states.contains(State::Enabled),
                selected: f.states.contains(State::Selected),
                checked,
                expanded,
                bounds: f.bounds,
                node_id: Some(id),
                children: Vec::new(),
                // AT-SPI exposes no application-level intent or bound state.
                semantic_intent: None,
                bound_state: None,
            };
            let idx = flat.len();
            flat.push(Flat { node, parent });
            // Actions on interactive leaves are what matters; do not descend
            // into a widget's implementation details past the depth cap.
            if depth < max_depth && f.interfaces.contains(Interface::Accessible) {
                let n = f.children.len();
                if n > MAX_CHILDREN {
                    partial = true;
                }
                for (i, child) in f.children.into_iter().take(MAX_CHILDREN).enumerate() {
                    if child.is_null() {
                        continue;
                    }
                    let mut p = path.clone();
                    p.push(i);
                    frontier.push((Some(idx), child, p, depth + 1));
                }
            }
        }
    }
    if !frontier.is_empty() {
        partial = true;
    }
    // Assemble: children were pushed after their parents, so a reverse pass
    // moves each subtree into its parent in one go.
    let count = flat.len();
    let mut nodes: Vec<Option<UiNode>> = Vec::with_capacity(count);
    let mut parents: Vec<Option<usize>> = Vec::with_capacity(count);
    for f in flat {
        nodes.push(Some(f.node));
        parents.push(f.parent);
    }
    // Children must keep tree order; collect per parent in index order.
    let mut by_parent: Vec<Vec<usize>> = vec![Vec::new(); count];
    for (i, p) in parents.iter().enumerate() {
        if let Some(p) = p {
            by_parent[*p].push(i);
        }
    }
    for i in (0..count).rev() {
        let kids: Vec<UiNode> = by_parent[i]
            .iter()
            .filter_map(|&c| nodes[c].take())
            .collect();
        if let Some(n) = nodes[i].as_mut() {
            n.children = kids;
        }
    }
    let root = nodes
        .into_iter()
        .next()
        .flatten()
        .unwrap_or_else(|| UiNode {
            role: "application".into(),
            ..Default::default()
        });
    Walked {
        root,
        paths,
        partial,
        count,
    }
}

/// Re-find a snapshot ref's widget: the stored object if it still answers,
/// else the child-index path, else a bounded search for the same role and
/// name under the application.
pub async fn relocate(
    conn: &AccessibilityConnection,
    r: &NodeRef,
) -> Result<ObjectRefOwned, String> {
    if let Ok(p) = proxy_for(conn, &r.obj).await {
        if let Ok(states) = p.get_state().await {
            if !states.contains(State::Defunct) {
                return Ok(r.obj.clone());
            }
        }
    }
    // Path replay.
    let mut cur = r.app.clone();
    let mut ok = true;
    for &i in &r.path {
        let Ok(p) = proxy_for(conn, &cur).await else {
            ok = false;
            break;
        };
        match p.get_child_at_index(i as i32).await {
            Ok(c) if !c.is_null() => cur = c,
            _ => {
                ok = false;
                break;
            }
        }
    }
    if ok {
        if let Ok(p) = proxy_for(conn, &cur).await {
            let (role, states) =
                futures_util::try_join!(p.get_role(), p.get_state()).map_err(|e| e.to_string())?;
            let name = p.name().await.unwrap_or_default();
            let (norm, _) = roles::normalize(role, states);
            let same_name = r.name.as_deref().unwrap_or("") == name;
            if norm == r.role && same_name && !states.contains(State::Defunct) {
                tracing::debug!("ref object was stale; re-found by path");
                return Ok(cur);
            }
        }
    }
    // Identity search.
    let started = Instant::now();
    let mut frontier = vec![r.app.clone()];
    let mut seen = 0usize;
    while let Some(obj) = frontier.pop() {
        if seen > 2000 || started.elapsed() > Duration::from_secs(2) {
            break;
        }
        seen += 1;
        let Ok(p) = proxy_for(conn, &obj).await else {
            continue;
        };
        let Ok((role, states, name, children)) =
            futures_util::try_join!(p.get_role(), p.get_state(), p.name(), p.get_children())
        else {
            continue;
        };
        let (norm, _) = roles::normalize(role, states);
        if norm == r.role && r.name.as_deref().unwrap_or("") == name {
            tracing::debug!("ref object was stale; re-found by identity");
            return Ok(obj);
        }
        frontier.extend(
            children
                .into_iter()
                .take(MAX_CHILDREN)
                .filter(|c| !c.is_null()),
        );
    }
    Err(format!(
        "element '{}' ({}) no longer exists; take a new snapshot",
        r.name.as_deref().unwrap_or(""),
        r.role
    ))
}

/// The action names AT-SPI toolkits use for "press this".
pub const PRESS_ACTIONS: &[&str] = &[
    "click", "activate", "press", "jump", "toggle", "select", "open", "menu",
];

/// Descendants matching a predicate, bounded, for dialog and menu sweeps.
pub async fn descendants(
    conn: &AccessibilityConnection,
    root: &ObjectRefOwned,
    max_depth: usize,
    max_nodes: usize,
) -> Vec<(ObjectRefOwned, Role, String, StateSet)> {
    let mut out = Vec::new();
    let mut frontier = vec![(root.clone(), 0usize)];
    let started = Instant::now();
    while let Some((obj, depth)) = frontier.pop() {
        if out.len() >= max_nodes || started.elapsed() > Duration::from_secs(2) {
            break;
        }
        let Ok(p) = proxy_for(conn, &obj).await else {
            continue;
        };
        let Ok((role, states, name, children)) =
            futures_util::try_join!(p.get_role(), p.get_state(), p.name(), p.get_children())
        else {
            continue;
        };
        out.push((obj, role, name, states));
        if depth < max_depth {
            for c in children.into_iter().take(MAX_CHILDREN).rev() {
                if !c.is_null() {
                    frontier.push((c, depth + 1));
                }
            }
        }
    }
    out
}
