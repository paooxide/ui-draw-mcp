//! The shared backend object: one struct implements perception, semantic
//! and coordinate input, and window control, so that a ref handed out by
//! `get_ui_tree` is something `ui_action` can act on.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atspi::{AccessibilityConnection, ObjectRefOwned};
use mcp_a11y::{A11yBackend, BackendError, RawSnapshot, SnapshotRequest};
use mcp_window::TargetSource;
use tokio::sync::OnceCell;

use crate::a11y::{self, AppInfo, NodeRef};
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
    /// Set by `focus_app`, or by a `get_ui_tree`-family call that names an
    /// `app` (or names none and finds nothing pinned), and then followed:
    /// later calls stay on it instead of tracking whatever happens to be
    /// frontmost. Window, menu and capture calls that name an app do not
    /// change it.
    pub target: Option<AppTarget>,
    /// Pointer positions this backend set, newest last.
    pub pointer_sets: Vec<mcp_input::SetPoint>,
    /// Modifiers held down by `key_down`. A pointer action or chord that
    /// names one of them must neither press it again nor let go of it when it
    /// finishes, or `shift` held across clicks would end at the first click.
    pub held_mods: Vec<crate::keys::Modifier>,
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
    /// Counts requests to stop in-flight work. Compared with the value a call
    /// started at, between the steps of a drag or keystrokes; never reset.
    pub(crate) takeovers: AtomicU64,
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
            takeovers: AtomicU64::new(0),
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

    /// Which app a call acts on and why that one, with nothing pinned or
    /// changed. A name given to the call wins; otherwise the pinned app while
    /// it is on the bus; otherwise the app with the active window. A name that
    /// matches no running app is an error, never another app.
    pub(crate) async fn peek_target(
        &self,
        app: Option<&str>,
    ) -> Result<(AppInfo, TargetSource), String> {
        let conn = self.conn().await?;
        let pinned = self.pinned_target().await;
        match mcp_window::target_source(app, pinned.is_some()) {
            TargetSource::Named => {
                let name = app.unwrap_or_default().trim();
                let found = a11y::app_by_name(&conn, name)
                    .await?
                    .ok_or_else(|| format!("no running application matching '{name}'"))?;
                Ok((found, TargetSource::Named))
            }
            TargetSource::Pinned => pinned
                .map(|a| (a, TargetSource::Pinned))
                .ok_or_else(|| "the pinned application is no longer running".to_string()),
            TargetSource::Frontmost => {
                let active = a11y::active_app(&conn)
                    .await?
                    .ok_or("no application has an active window")?;
                Ok((active, TargetSource::Frontmost))
            }
        }
    }

    /// Resolve which app to drive, and remember it. An explicit name wins and
    /// becomes the pinned target; otherwise the pinned target while it is
    /// still on the bus; otherwise the app with the active window, which then
    /// becomes the pin. For perception and the actions on its refs; window,
    /// menu and capture calls use [`Self::peek_target`] and pin nothing.
    pub(crate) async fn resolve_target(
        &self,
        app: Option<&str>,
    ) -> Result<(AppInfo, TargetSource), String> {
        let (found, from) = self.peek_target(app).await?;
        if from != TargetSource::Pinned {
            self.set_target(&found);
        }
        Ok((found, from))
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
        // A passive read (a wait, a check) names the app it reads without
        // becoming it, and keeps the refs of the last walk.
        let (app, from) = if req.passive {
            self.peek_target(req.app.as_deref()).await
        } else {
            self.resolve_target(req.app.as_deref()).await
        }
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
        if !req.passive {
            let mut st = self.lock();
            st.paths = walked.paths;
        }
        let terminal = is_terminal(&app.name) || walked.root.subrole.as_deref() == Some("terminal");
        let pid = a11y::pid_of(&conn, &app.obj).await;
        Ok(RawSnapshot {
            root: walked.root,
            app: Some(app.name),
            window,
            terminal_app: terminal,
            partial: walked.partial,
            target: Some(mcp_a11y::SnapshotTarget {
                pid,
                from: from.as_str(),
            }),
        })
    }

    fn platform(&self) -> &'static str {
        "linux"
    }
}
