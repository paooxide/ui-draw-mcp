//! The `RemoteDesktop` portal session that carries every synthetic keystroke
//! and pointer event.
//!
//! Wayland compositors do not let a client inject input into other clients,
//! by design. The portal is the sanctioned exception: the compositor shows
//! the human a dialog naming the requesting program and the devices it wants,
//! and only a session the human approved can inject. That dialog is out of
//! band from the agent, which cannot see or answer it. The approval is
//! persisted with a restore token so the human is asked once, not per call,
//! and the token file lives beside the rest of agentctl's state.
//!
//! Absolute pointer motion is addressed to a screen-cast stream, so the
//! session also selects every monitor as a source. No frames are ever read
//! from those streams; they exist so a coordinate has something to be
//! relative to.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ashpd::desktop::remote_desktop::{Axis, DeviceType, KeyState, RemoteDesktop};
use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
use ashpd::desktop::{PersistMode, Session};
use enumflags2::BitFlags;
use futures_util::future::BoxFuture;
use futures_util::FutureExt;
use mcp_input::InputError;
use tokio::sync::Mutex;
use xkeysym::Keysym;

/// How long the human has to answer the portal dialog before the call fails
/// closed. Long, because a person may be away from the desk; bounded, because
/// a tool call cannot hang forever.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);

/// One monitor's stream: where it sits in logical screen space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StreamGeom {
    pub node: u32,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl StreamGeom {
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }
}

/// Pick the stream a screen point falls in, else the nearest one, so a point
/// just past an edge still lands on a monitor instead of failing.
pub fn stream_for(streams: &[StreamGeom], x: f64, y: f64) -> Option<(StreamGeom, f64, f64)> {
    if let Some(s) = streams.iter().find(|s| s.contains(x, y)) {
        return Some((*s, x - s.x, y - s.y));
    }
    let s = streams.iter().min_by(|a, b| {
        let da = dist2(a, x, y);
        let db = dist2(b, x, y);
        da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
    })?;
    let cx = x.clamp(s.x, s.x + s.w - 1.0) - s.x;
    let cy = y.clamp(s.y, s.y + s.h - 1.0) - s.y;
    Some((*s, cx, cy))
}

fn dist2(s: &StreamGeom, x: f64, y: f64) -> f64 {
    let dx = if x < s.x {
        s.x - x
    } else if x >= s.x + s.w {
        x - (s.x + s.w)
    } else {
        0.0
    };
    let dy = if y < s.y {
        s.y - y
    } else if y >= s.y + s.h {
        y - (s.y + s.h)
    } else {
        0.0
    };
    dx * dx + dy * dy
}

/// Does a portal error say the compositor will not persist the session? The
/// message is the only signal ashpd surfaces for it.
pub fn refused_persistence(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("cannot persist") || (m.contains("persist") && m.contains("invalidargument"))
}

/// evdev button codes the portal expects.
pub fn button_code(name: Option<&str>) -> Result<i32, InputError> {
    Ok(match name.unwrap_or("left") {
        "left" => 0x110,
        "right" => 0x111,
        "middle" => 0x112,
        other => {
            return Err(InputError::Failed(format!(
                "unknown button '{other}' (left, right, middle)"
            )))
        }
    })
}

struct Live {
    proxy: RemoteDesktop<'static>,
    session: Session<'static, RemoteDesktop<'static>>,
    streams: Vec<StreamGeom>,
}

/// How often the monitor layout is compared against the session's streams.
const LAYOUT_CHECK_EVERY: Duration = Duration::from_secs(3);

/// A monitor layout, as rectangles in logical screen space.
pub type Layout = Vec<(f64, f64, f64, f64)>;

/// Do the session's streams still describe the monitors that exist? A
/// monitor plugged in or removed after the session opened leaves a stream
/// for a screen that is gone, or no stream for one that is new; either way
/// absolute motion would land on the wrong screen, so the session is
/// reopened (the remembered grant means no dialog).
pub fn layout_matches(streams: &[StreamGeom], layout: &Layout) -> bool {
    if layout.is_empty() {
        // No geometry source: nothing to compare against, keep the session.
        return true;
    }
    if streams.len() != layout.len() {
        return false;
    }
    layout.iter().all(|&(x, y, w, h)| {
        streams
            .iter()
            .any(|s| s.x == x && s.y == y && s.w == w && s.h == h)
    })
}

pub struct Portal {
    live: Mutex<Option<Live>>,
    token_path: PathBuf,
    /// When the layout was last compared, so a drag's many moves do not
    /// each cost a D-Bus round trip.
    layout_checked: Mutex<Option<Instant>>,
}

impl Portal {
    pub fn new(state_dir: PathBuf) -> Self {
        Portal {
            live: Mutex::new(None),
            token_path: state_dir.join("portal-restore-token"),
            layout_checked: Mutex::new(None),
        }
    }

    /// Drop the session if the monitors changed since it opened.
    async fn reopen_if_layout_changed(&self) {
        {
            let mut checked = self.layout_checked.lock().await;
            if checked.is_some_and(|t| t.elapsed() < LAYOUT_CHECK_EVERY) {
                return;
            }
            *checked = Some(Instant::now());
        }
        let layout: Layout = match crate::vision::monitors_public().await {
            Ok(ms) => ms
                .iter()
                .map(|m| (m.x as f64, m.y as f64, m.w as f64, m.h as f64))
                .collect(),
            Err(_) => return,
        };
        let mut guard = self.live.lock().await;
        if let Some(live) = guard.as_ref() {
            if !layout_matches(&live.streams, &layout) {
                tracing::info!(
                    streams = live.streams.len(),
                    monitors = layout.len(),
                    "monitor layout changed; reopening the remote-desktop session"
                );
                let _ = live.session.close().await;
                *guard = None;
            }
        }
    }

    fn read_token(&self) -> Option<String> {
        std::fs::read_to_string(&self.token_path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    fn write_token(&self, token: &str) {
        if let Some(dir) = self.token_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&self.token_path, token) {
            tracing::warn!(error = %e, path = %self.token_path.display(), "could not save the portal restore token; the human will be asked again next run");
            return;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                std::fs::set_permissions(&self.token_path, std::fs::Permissions::from_mode(0o600));
        }
    }

    /// Open a session. GNOME refuses persistence on a remote-desktop session
    /// ("Remote desktop sessions cannot persist"), while KDE and newer GNOME
    /// allow it, so this tries to persist first and falls back to a
    /// per-session grant when the compositor rejects the mode. A persisted
    /// grant is remembered; a per-session one asks the human each run.
    async fn open(&self) -> Result<Live, InputError> {
        match self.open_with(PersistMode::ExplicitlyRevoked).await {
            Err(InputError::Failed(m)) if refused_persistence(&m) => {
                tracing::info!(
                    "this compositor does not persist remote-desktop grants;                      asking for a per-session one instead"
                );
                self.open_with(PersistMode::DoNot).await
            }
            other => other,
        }
    }

    async fn open_with(&self, persist: PersistMode) -> Result<Live, InputError> {
        let failed = |what: &str, e: ashpd::Error| {
            InputError::Failed(format!("remote-desktop portal {what}: {e}"))
        };
        let proxy = RemoteDesktop::new().await.map_err(|e| {
            InputError::Unsupported(format!(
                "no RemoteDesktop portal on this session (is xdg-desktop-portal running?): {e}"
            ))
        })?;
        let session = proxy
            .create_session()
            .await
            .map_err(|e| failed("create session", e))?;
        // A restore token only helps when we are asking to persist; with a
        // per-session grant there is nothing to restore.
        let token = if persist == PersistMode::DoNot {
            None
        } else {
            self.read_token()
        };
        proxy
            .select_devices(
                &session,
                DeviceType::Keyboard | DeviceType::Pointer,
                token.as_deref(),
                persist,
            )
            .await
            .map_err(|e| failed("select devices", e))?
            .response()
            .map_err(|e| failed("select devices", e))?;
        // Every monitor, so absolute motion has a stream to be relative to.
        let cast = Screencast::new()
            .await
            .map_err(|e| failed("screencast", e))?;
        cast.select_sources(
            &session,
            CursorMode::Hidden,
            BitFlags::from(SourceType::Monitor),
            true,
            token.as_deref(),
            persist,
        )
        .await
        .map_err(|e| failed("select sources", e))?
        .response()
        .map_err(|e| failed("select sources", e))?;
        let started = tokio::time::timeout(APPROVAL_TIMEOUT, proxy.start(&session, None))
            .await
            .map_err(|_| {
                InputError::PermissionDenied(
                    "nobody answered the remote-desktop approval dialog within two minutes".into(),
                )
            })?
            .map_err(|e| failed("start", e))?;
        let devices = match started.response() {
            Ok(d) => d,
            Err(ashpd::Error::Response(r)) => {
                return Err(InputError::PermissionDenied(format!(
                    "the remote-desktop approval dialog was not accepted ({r:?}); synthetic input is refused"
                )))
            }
            Err(e) => return Err(failed("start", e)),
        };
        if !devices.devices().contains(DeviceType::Keyboard)
            || !devices.devices().contains(DeviceType::Pointer)
        {
            return Err(InputError::PermissionDenied(format!(
                "the portal granted {:?}, not keyboard and pointer",
                devices.devices()
            )));
        }
        if persist != PersistMode::DoNot {
            if let Some(t) = devices.restore_token() {
                self.write_token(t);
            }
        }
        let streams: Vec<StreamGeom> = devices
            .streams()
            .unwrap_or(&[])
            .iter()
            .map(|s| {
                let (x, y) = s.position().unwrap_or((0, 0));
                let (w, h) = s.size().unwrap_or((0, 0));
                StreamGeom {
                    node: s.pipe_wire_node_id(),
                    x: x as f64,
                    y: y as f64,
                    w: w as f64,
                    h: h as f64,
                }
            })
            .collect();
        tracing::info!(
            streams = streams.len(),
            "remote-desktop portal session started"
        );
        Ok(Live {
            proxy,
            session,
            streams,
        })
    }

    /// Run `f` against the live session, opening one first if needed. A
    /// session the compositor closed (the human revoked it, or the portal
    /// restarted) is dropped so the next call asks again.
    async fn with<T, F>(&self, f: F) -> Result<T, InputError>
    where
        F: for<'a> FnOnce(
            &'a RemoteDesktop<'static>,
            &'a Session<'static, RemoteDesktop<'static>>,
            &'a [StreamGeom],
        ) -> BoxFuture<'a, Result<T, ashpd::Error>>,
    {
        let mut guard = self.live.lock().await;
        if guard.is_none() {
            *guard = Some(self.open().await?);
        }
        let live = guard.as_ref().expect("just opened");
        match f(&live.proxy, &live.session, &live.streams).await {
            Ok(v) => Ok(v),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("UnknownObject")
                    || msg.contains("UnknownMethod")
                    || msg.contains("session")
                {
                    tracing::warn!(error = %msg, "remote-desktop session is gone; it will be reopened");
                    *guard = None;
                }
                Err(InputError::Failed(format!("remote-desktop portal: {msg}")))
            }
        }
    }

    /// Open the session if it is not already open, so the approval dialog is
    /// dealt with before any keystroke is sent. Opening it lazily on the first
    /// key means the dialog steals focus mid-type; warming it up first lets the
    /// caller settle focus on the target after the dialog closes.
    pub async fn ensure_ready(&self) -> Result<(), InputError> {
        let mut guard = self.live.lock().await;
        if guard.is_none() {
            *guard = Some(self.open().await?);
        }
        Ok(())
    }

    pub async fn key(&self, sym: Keysym, pressed: bool) -> Result<(), InputError> {
        let state = if pressed {
            KeyState::Pressed
        } else {
            KeyState::Released
        };
        self.with(move |p, s, _| {
            async move { p.notify_keyboard_keysym(s, sym.raw() as i32, state).await }.boxed()
        })
        .await
    }

    pub async fn pointer_abs(&self, x: f64, y: f64) -> Result<(), InputError> {
        self.reopen_if_layout_changed().await;
        self.with(move |p, s, streams| {
            async move {
                let Some((stream, rx, ry)) = stream_for(streams, x, y) else {
                    return Err(ashpd::Error::NoResponse);
                };
                p.notify_pointer_motion_absolute(s, stream.node, rx, ry).await
            }
            .boxed()
        })
        .await
        .map_err(|e| match e {
            InputError::Failed(m) if m.contains("NoResponse") || m.contains("no response") => {
                InputError::Unsupported(
                    "the portal session has no monitor stream, so absolute pointer motion is not possible; approve screen sharing when the dialog asks".into(),
                )
            }
            other => other,
        })
    }

    pub async fn button(&self, code: i32, pressed: bool) -> Result<(), InputError> {
        let state = if pressed {
            KeyState::Pressed
        } else {
            KeyState::Released
        };
        self.with(move |p, s, _| {
            async move { p.notify_pointer_button(s, code, state).await }.boxed()
        })
        .await
    }

    pub async fn axis_discrete(&self, vertical: bool, steps: i32) -> Result<(), InputError> {
        let axis = if vertical {
            Axis::Vertical
        } else {
            Axis::Horizontal
        };
        self.with(move |p, s, _| {
            async move { p.notify_pointer_axis_discrete(s, axis, steps).await }.boxed()
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geoms() -> Vec<StreamGeom> {
        vec![
            StreamGeom {
                node: 1,
                x: 0.0,
                y: 0.0,
                w: 1920.0,
                h: 1080.0,
            },
            StreamGeom {
                node: 2,
                x: 1920.0,
                y: 0.0,
                w: 1920.0,
                h: 1080.0,
            },
        ]
    }

    #[test]
    fn points_map_to_the_monitor_they_fall_in() {
        let (s, rx, ry) = stream_for(&geoms(), 10.0, 20.0).unwrap();
        assert_eq!((s.node, rx, ry), (1, 10.0, 20.0));
        let (s, rx, ry) = stream_for(&geoms(), 1930.0, 20.0).unwrap();
        assert_eq!((s.node, rx, ry), (2, 10.0, 20.0));
        // Boundary: the first pixel of the second monitor is the second's.
        assert_eq!(stream_for(&geoms(), 1920.0, 0.0).unwrap().0.node, 2);
        assert_eq!(stream_for(&geoms(), 1919.0, 1079.0).unwrap().0.node, 1);
    }

    #[test]
    fn points_off_every_monitor_clamp_to_the_nearest() {
        let (s, rx, ry) = stream_for(&geoms(), -50.0, -50.0).unwrap();
        assert_eq!((s.node, rx, ry), (1, 0.0, 0.0));
        let (s, rx, ry) = stream_for(&geoms(), 5000.0, 2000.0).unwrap();
        assert_eq!((s.node, rx, ry), (2, 1919.0, 1079.0));
        assert!(stream_for(&[], 1.0, 1.0).is_none());
    }

    #[test]
    fn a_changed_monitor_layout_is_detected_and_no_geometry_is_not() {
        let two: Layout = vec![(0.0, 0.0, 1920.0, 1080.0), (1920.0, 0.0, 1920.0, 1080.0)];
        assert!(layout_matches(&geoms(), &two));
        // Order does not matter.
        let swapped: Layout = vec![two[1], two[0]];
        assert!(layout_matches(&geoms(), &swapped));
        // A monitor unplugged.
        assert!(!layout_matches(&geoms(), &vec![two[0]]));
        // A monitor moved.
        let moved: Layout = vec![two[0], (1920.0, 200.0, 1920.0, 1080.0)];
        assert!(!layout_matches(&geoms(), &moved));
        // A monitor added.
        let three: Layout = vec![two[0], two[1], (3840.0, 0.0, 1280.0, 1024.0)];
        assert!(!layout_matches(&geoms(), &three));
        // No geometry source (not GNOME): keep the session rather than churn.
        assert!(layout_matches(&geoms(), &Vec::new()));
        // No streams yet and monitors exist: mismatch, so a session is opened.
        assert!(!layout_matches(&[], &two));
    }

    #[test]
    fn a_persistence_refusal_is_recognised_from_the_portal_message() {
        assert!(refused_persistence(
            "remote-desktop portal select sources: Portal request failed: org.freedesktop.portal.Error.InvalidArgument: Remote desktop sessions cannot persist"
        ));
        assert!(refused_persistence(
            "something InvalidArgument about persist mode"
        ));
        assert!(!refused_persistence(
            "select devices: the dialog was dismissed"
        ));
        assert!(!refused_persistence("connection reset"));
    }

    #[test]
    fn button_names_map_to_evdev_codes() {
        assert_eq!(button_code(None).unwrap(), 0x110);
        assert_eq!(button_code(Some("right")).unwrap(), 0x111);
        assert_eq!(button_code(Some("middle")).unwrap(), 0x112);
        assert!(button_code(Some("x1")).is_err());
    }
}
