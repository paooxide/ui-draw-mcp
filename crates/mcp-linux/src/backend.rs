//! The shared backend object: one struct implements perception, semantic
//! and coordinate input, and window control, so that a ref handed out by
//! `get_ui_tree` is something `ui_action` can act on.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atspi::{AccessibilityConnection, ObjectRefOwned};
use mcp_a11y::{A11yBackend, BackendError, RawSnapshot, SnapshotRequest};
use tokio::sync::OnceCell;

use crate::a11y::{self, AppInfo, NodeRef, WindowRef};
use crate::portal::Portal;

/// The application this session is driving.
#[derive(Debug, Clone)]
pub struct AppTarget {
    pub obj: ObjectRefOwned,
    pub name: String,
}

#[derive(Default)]
pub struct State {
    pub paths: HashMap<u64, NodeRef>,
    /// Set by an explicit `app` argument and then followed: later calls stay
    /// on it instead of tracking whatever happens to be frontmost.
    pub target: Option<AppTarget>,
    /// Windows as of the last `list_windows`, by the id it reported.
    pub windows: Vec<WindowRef>,
    /// Pointer positions this backend set, newest last.
    pub pointer_sets: Vec<mcp_input::SetPoint>,
}

/// Applications whose text is a scrollback: keep value tails, not heads.
pub fn is_terminal(app: &str) -> bool {
    const TERMS: &[&str] = &[
        "terminal",
        "ptyxis",
        "console",
        "konsole",
        "alacritty",
        "kitty",
        "foot",
        "wezterm",
        "xterm",
        "tilix",
        "terminator",
        "ghostty",
        "warp",
    ];
    let lower = app.to_ascii_lowercase();
    TERMS.iter().any(|t| lower.contains(t))
}

/// `~/.agentctl/bin`, or the temp directory when there is no home.
fn default_helper_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(".agentctl")
        .join("bin")
}

pub struct LinuxBackend {
    conn: OnceCell<Arc<AccessibilityConnection>>,
    pub(crate) state: Mutex<State>,
    pub(crate) portal: Portal,
    /// Where downloaded OCR models and the portal restore token live.
    pub(crate) helper_dir: PathBuf,
    /// Set when something asks in-flight work to stop. Checked between the
    /// steps of a drag.
    pub(crate) cancel: AtomicBool,
    pub(crate) ocr: OnceCell<Arc<crate::vision::Ocr>>,
}

impl Default for LinuxBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl LinuxBackend {
    pub fn new() -> Self {
        let helper_dir = default_helper_dir();
        LinuxBackend {
            conn: OnceCell::new(),
            state: Mutex::new(State::default()),
            portal: Portal::new(helper_dir.clone()),
            helper_dir,
            cancel: AtomicBool::new(false),
            ocr: OnceCell::new(),
        }
    }

    /// Keep state (OCR models, the portal restore token) somewhere other
    /// than the default.
    pub fn with_helper_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.helper_dir = dir.into();
        self.portal = Portal::new(self.helper_dir.clone());
        self
    }

    /// The accessibility bus, connected on first use and kept.
    pub(crate) async fn conn(&self) -> Result<Arc<AccessibilityConnection>, String> {
        self.conn
            .get_or_try_init(|| async { a11y::connect().await.map(Arc::new) })
            .await
            .cloned()
    }

    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn set_target(&self, app: &AppInfo) {
        self.lock().target = Some(AppTarget {
            obj: app.obj.clone(),
            name: app.name.clone(),
        });
    }

    /// Forget the sticky target (next call tracks the active app again).
    pub fn clear_target(&self) {
        self.lock().target = None;
    }

    pub fn current_target(&self) -> Option<String> {
        self.lock().target.as_ref().map(|t| t.name.clone())
    }

    /// Resolve which app to drive, and remember it. An explicit name wins and
    /// becomes the sticky target; otherwise the sticky target while it is
    /// still on the bus; otherwise the app with the active window.
    pub(crate) async fn resolve_target(&self, app: Option<&str>) -> Result<AppInfo, String> {
        let conn = self.conn().await?;
        if let Some(name) = app.map(str::trim).filter(|s| !s.is_empty()) {
            let found = a11y::app_by_name(&conn, name)
                .await?
                .ok_or_else(|| format!("no running application matching '{name}'"))?;
            self.set_target(&found);
            return Ok(found);
        }
        let sticky = self.lock().target.clone();
        if let Some(t) = sticky {
            let apps = a11y::applications(&conn).await?;
            if let Some(a) = apps.iter().find(|a| a.obj == t.obj) {
                return Ok(a.clone());
            }
            // Target went away: drop it and fall through.
            self.lock().target = None;
        }
        let active = a11y::active_app(&conn)
            .await?
            .ok_or("no application has an active window")?;
        self.set_target(&active);
        Ok(active)
    }

    /// The pinned target if it is still on the bus.
    pub(crate) async fn pinned_target(&self) -> Option<AppInfo> {
        let t = self.lock().target.clone()?;
        let conn = self.conn().await.ok()?;
        let apps = a11y::applications(&conn).await.ok()?;
        apps.into_iter().find(|a| a.obj == t.obj)
    }

    pub(crate) fn node_ref(&self, node_id: u64) -> Result<NodeRef, mcp_input::InputError> {
        self.lock().paths.get(&node_id).cloned().ok_or_else(|| {
            mcp_input::InputError::NotFound(format!(
                "no element with id {node_id} in the current snapshot; take a new one"
            ))
        })
    }
}

#[async_trait]
impl A11yBackend for LinuxBackend {
    async fn snapshot(&self, req: &SnapshotRequest) -> Result<RawSnapshot, BackendError> {
        let conn = self.conn().await.map_err(BackendError::PermissionDenied)?;
        let app = self
            .resolve_target(req.app.as_deref())
            .await
            .map_err(BackendError::NotFound)?;
        let root = match &req.root_ref {
            Some(r) => {
                let id: u64 = r
                    .trim_start_matches("@e")
                    .parse()
                    .map_err(|_| BackendError::NotFound(format!("bad root_ref '{r}'")))?;
                let nref = self.lock().paths.get(&id).cloned().ok_or_else(|| {
                    BackendError::NotFound(format!("root_ref '{r}' is not in the current snapshot"))
                })?;
                a11y::relocate(&conn, &nref)
                    .await
                    .map_err(BackendError::NotFound)?
            }
            None => app.obj.clone(),
        };
        let max_depth = req.max_depth.unwrap_or(40);
        let walked = a11y::walk(
            conn.clone(),
            app.obj.clone(),
            root,
            max_depth,
            a11y::WALK_BUDGET,
        )
        .await;
        if walked.count == 0 {
            return Err(BackendError::Failed(format!(
                "'{}' exposes no accessibility tree",
                app.name
            )));
        }
        // The window title of the active frame, when the app has one.
        let window = a11y::windows_of(&conn, &app)
            .await
            .ok()
            .and_then(|ws| {
                ws.iter()
                    .find(|w| w.states.contains(atspi::State::Active))
                    .or(ws.first())
                    .map(|w| w.title.clone())
            })
            .filter(|t| !t.is_empty());
        {
            let mut st = self.lock();
            st.paths = walked.paths;
        }
        let terminal = is_terminal(&app.name) || walked.root.subrole.as_deref() == Some("terminal");
        Ok(RawSnapshot {
            root: walked.root,
            app: Some(app.name),
            window,
            terminal_app: terminal,
            partial: walked.partial,
        })
    }

    fn platform(&self) -> &'static str {
        "linux"
    }
}
