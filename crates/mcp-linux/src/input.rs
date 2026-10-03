//! Input: semantic actions through AT-SPI, everything else through the
//! portal.
//!
//! A semantic action (`ui_action` on a ref) asks the widget itself to do the
//! thing, via its AT-SPI `Action` interface, with no pointer involved. That
//! is the reliable path on Wayland, where a toolkit's reported coordinates
//! are window-relative and the compositor never tells a client where a
//! window is. Coordinate input exists for surfaces with no tree, and its
//! coordinates are logical screen pixels, the space `list_displays` and the
//! screenshot mapping describe.

use std::sync::atomic::Ordering;
use std::time::Duration;

use async_trait::async_trait;
use atspi::proxy::proxy_ext::ProxyExt;
use atspi::{Interface, State};
use mcp_input::{
    ClipData, ClipFormat, InputBackend, InputError, MouseKind, ScrollDir, SemanticAction, SetPoint,
};
use tokio::time::sleep;
use xkeysym::Keysym;

use crate::a11y;
use crate::backend::LinuxBackend;
use crate::clip;
use crate::keys::{self, Modifier};
use crate::portal::button_code;

/// Gap between a press and its release, and between successive keys. Real
/// keyboards have one, and some toolkits drop a press and release that
/// arrive in the same frame.
const KEY_GAP: Duration = Duration::from_millis(6);
/// Gap between the clicks of a double click; well inside every toolkit's
/// double-click interval.
const CLICK_GAP: Duration = Duration::from_millis(60);
/// How many pointer positions to remember for the override watcher.
const MAX_POINTER_SETS: usize = 64;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl LinuxBackend {
    fn note_pointer(&self, x: f64, y: f64) {
        let mut st = self.lock();
        st.pointer_sets.push(SetPoint {
            x,
            y,
            at_ms: now_ms(),
        });
        if st.pointer_sets.len() > MAX_POINTER_SETS {
            let excess = st.pointer_sets.len() - MAX_POINTER_SETS;
            st.pointer_sets.drain(..excess);
        }
    }

    async fn press_modifiers(&self, mods: &[Modifier], down: bool) -> Result<(), InputError> {
        let order: Vec<Modifier> = if down {
            mods.to_vec()
        } else {
            mods.iter().rev().copied().collect()
        };
        for m in order {
            self.portal.key(m.keysym(), down).await?;
            sleep(KEY_GAP).await;
        }
        Ok(())
    }

    async fn tap(&self, sym: Keysym) -> Result<(), InputError> {
        self.portal.key(sym, true).await?;
        sleep(KEY_GAP).await;
        self.portal.key(sym, false).await?;
        sleep(KEY_GAP).await;
        Ok(())
    }

    async fn click_at(
        &self,
        x: f64,
        y: f64,
        code: i32,
        times: u32,
        mods: &[Modifier],
    ) -> Result<(), InputError> {
        self.portal.pointer_abs(x, y).await?;
        self.note_pointer(x, y);
        sleep(Duration::from_millis(20)).await;
        self.press_modifiers(mods, true).await?;
        for i in 0..times {
            self.portal.button(code, true).await?;
            sleep(Duration::from_millis(30)).await;
            self.portal.button(code, false).await?;
            if i + 1 < times {
                sleep(CLICK_GAP).await;
            }
        }
        self.press_modifiers(mods, false).await?;
        Ok(())
    }

    /// Bring the target application's window forward before typing, so
    /// keystrokes land where the snapshot came from rather than in whatever
    /// the person last clicked. Best effort, and verified: refuses to type
    /// when the target could not be raised.
    async fn ensure_target_active(&self) -> Result<(), InputError> {
        let Some(target) = self.pinned_target().await else {
            tracing::debug!("no pinned target; keys go to whatever the compositor has focused");
            return Ok(());
        };
        let conn = self.conn().await.map_err(InputError::Failed)?;
        let is_active =
            |ws: &[a11y::WindowRef]| ws.iter().any(|w| w.states.contains(State::Active));
        let app = target.clone();
        if let Ok(ws) = a11y::windows_of(&conn, &app).await {
            if is_active(&ws) {
                return Ok(());
            }
        }
        crate::window::activate_app(&conn, &app).await?;
        for _ in 0..24 {
            sleep(Duration::from_millis(50)).await;
            if let Ok(ws) = a11y::windows_of(&conn, &app).await {
                if is_active(&ws) {
                    tracing::debug!(app = %target.name, "target raised and active");
                    // Active in the tree is not yet keyboard focus at the
                    // compositor; let the raise settle before keys are sent.
                    sleep(Duration::from_millis(120)).await;
                    return Ok(());
                }
            }
        }
        // Fail safe: keystrokes go to whatever the compositor has focused, so
        // typing without confirming the target is active would leak input into
        // another application. Refuse, and name the focus-independent paths.
        Err(InputError::Failed(format!(
            "refusing to type: {}",
            crate::window::cannot_foreground_msg(&target.name)
        )))
    }
}

#[async_trait]
impl InputBackend for LinuxBackend {
    async fn perform(
        &self,
        node_id: u64,
        action: SemanticAction,
        _option: Option<&str>,
    ) -> Result<(), InputError> {
        let conn = self.conn().await.map_err(InputError::Failed)?;
        let nref = self.node_ref(node_id)?;
        let obj = a11y::relocate(&conn, &nref)
            .await
            .map_err(InputError::NotFound)?;
        let p = a11y::proxy_for(&conn, &obj)
            .await
            .map_err(InputError::Failed)?;
        let interfaces = p
            .get_interfaces()
            .await
            .map_err(|e| InputError::Failed(format!("interfaces: {e}")))?;
        let label = nref.name.clone().unwrap_or_default();
        match action {
            SemanticAction::Focus => {
                if !interfaces.contains(Interface::Component) {
                    return Err(InputError::Unsupported(format!(
                        "'{label}' cannot take focus (no Component interface)"
                    )));
                }
                let c = component_iface(&p).await?;
                let ok = c
                    .grab_focus()
                    .await
                    .map_err(|e| InputError::Failed(format!("focus: {e}")))?;
                if ok {
                    Ok(())
                } else {
                    Err(InputError::Failed(format!("'{label}' refused focus")))
                }
            }
            SemanticAction::ScrollIntoView => {
                let c = component_iface(&p).await?;
                let ok = c
                    .scroll_to(atspi::ScrollType::Anywhere)
                    .await
                    .map_err(|e| InputError::Failed(format!("scroll_into_view: {e}")))?;
                if ok {
                    Ok(())
                } else {
                    Err(InputError::Failed(format!(
                        "'{label}' could not be scrolled into view"
                    )))
                }
            }
            SemanticAction::Expand | SemanticAction::Collapse => {
                // Toolkits expose expand/collapse as the same toggle action;
                // check the state first so the request is idempotent.
                let states = p
                    .get_state()
                    .await
                    .map_err(|e| InputError::Failed(e.to_string()))?;
                let expanded = states.contains(State::Expanded);
                let want = action == SemanticAction::Expand;
                if expanded == want {
                    return Ok(());
                }
                do_press(&p, interfaces, &label).await
            }
            SemanticAction::Check | SemanticAction::Uncheck => {
                let states = p
                    .get_state()
                    .await
                    .map_err(|e| InputError::Failed(e.to_string()))?;
                let checked = states.contains(State::Checked);
                let want = action == SemanticAction::Check;
                if checked == want {
                    return Ok(());
                }
                do_press(&p, interfaces, &label).await
            }
            SemanticAction::Select => {
                // Prefer the parent's Selection interface for list and tree
                // rows; fall back to the row's own action.
                if let Ok(parent) = p.parent().await {
                    if !parent.is_null() {
                        if let Ok(pp) = a11y::proxy_for(&conn, &parent).await {
                            if let Ok(sel) = selection_iface(&pp).await {
                                if let Ok(idx) = p.get_index_in_parent().await {
                                    if sel.select_child(idx).await.unwrap_or(false) {
                                        return Ok(());
                                    }
                                }
                            }
                        }
                    }
                }
                do_press(&p, interfaces, &label).await
            }
            SemanticAction::RightClick => {
                // AT-SPI has no context-menu action; do it with the pointer at
                // the widget's centre when the toolkit reports screen bounds,
                // which X11 and XWayland apps do and Wayland-native ones do not.
                let bounds = a11y::extents(&p).await.ok_or_else(|| {
                    InputError::Unsupported(format!(
                        "'{label}' reports no screen position, so a right click cannot be aimed; use mouse_action with coordinates from a screenshot"
                    ))
                })?;
                let (cx, cy) = (bounds.x + bounds.w / 2.0, bounds.y + bounds.h / 2.0);
                self.click_at(cx, cy, 0x111, 1, &[]).await
            }
            SemanticAction::Click | SemanticAction::DoubleClick | SemanticAction::Toggle => {
                do_press(&p, interfaces, &label).await
            }
        }
    }

    async fn set_value(&self, node_id: u64, text: &str) -> Result<(), InputError> {
        let conn = self.conn().await.map_err(InputError::Failed)?;
        let nref = self.node_ref(node_id)?;
        let obj = a11y::relocate(&conn, &nref)
            .await
            .map_err(InputError::NotFound)?;
        let p = a11y::proxy_for(&conn, &obj)
            .await
            .map_err(InputError::Failed)?;
        let interfaces = p
            .get_interfaces()
            .await
            .map_err(|e| InputError::Failed(format!("interfaces: {e}")))?;
        let label = nref.name.clone().unwrap_or_default();
        if interfaces.contains(Interface::EditableText) {
            let et = editable_text_iface(&p).await?;
            let ok = et
                .set_text_contents(text)
                .await
                .map_err(|e| InputError::Failed(format!("set_value: {e}")))?;
            if ok {
                return Ok(());
            }
            // Some toolkits answer false and still need the long way round.
            let t = text_iface(&p).await?;
            let n = t.character_count().await.unwrap_or(0);
            let _ = et.delete_text(0, n).await;
            let ok = et
                .insert_text(0, text, text.chars().count() as i32)
                .await
                .map_err(|e| InputError::Failed(format!("set_value: {e}")))?;
            if ok {
                return Ok(());
            }
            return Err(InputError::Failed(format!(
                "'{label}' rejected the new text (read-only, or the toolkit declined)"
            )));
        }
        if interfaces.contains(Interface::Value) {
            let v: f64 = text.trim().parse().map_err(|_| {
                InputError::Failed(format!("'{label}' takes a number, got '{text}'"))
            })?;
            let vp = value_iface(&p).await?;
            return vp
                .set_current_value(v)
                .await
                .map_err(|e| InputError::Failed(format!("set_value: {e}")));
        }
        Err(InputError::Unsupported(format!(
            "'{label}' ({}) is not editable; focus it and use keyboard_type",
            nref.role
        )))
    }

    async fn type_text(&self, text: &str) -> Result<(), InputError> {
        // Open the portal (and answer its dialog) before settling focus: the
        // dialog steals focus, so confirming the target first would be stale
        // by the time the first key is sent.
        let since = self.takeover_generation();
        self.portal.ensure_ready().await?;
        self.ensure_target_active().await?;
        tracing::debug!(
            target = ?self.current_target(),
            chars = text.chars().count(),
            "typing into the target"
        );
        for sym in keys::keysyms_for_text(text) {
            if self.takeover_generation() != since {
                return Err(InputError::Failed("typing aborted".into()));
            }
            self.tap(sym).await?;
        }
        Ok(())
    }

    async fn key_combo(&self, combo: &str) -> Result<(), InputError> {
        let parsed = keys::parse_combo(combo).map_err(InputError::Failed)?;
        self.portal.ensure_ready().await?;
        self.ensure_target_active().await?;
        self.press_modifiers(&parsed.modifiers, true).await?;
        let r = self.tap(parsed.key).await;
        // Always release what was pressed, even if the key itself failed.
        let released = self.press_modifiers(&parsed.modifiers, false).await;
        r.and(released)
    }

    async fn mouse(
        &self,
        kind: MouseKind,
        x: f64,
        y: f64,
        button: Option<&str>,
        modifiers: &[String],
    ) -> Result<(), InputError> {
        let mods = keys::pointer_modifiers(modifiers).map_err(InputError::Failed)?;
        let code = button_code(button)?;
        match kind {
            MouseKind::Move => {
                self.portal.pointer_abs(x, y).await?;
                self.note_pointer(x, y);
                Ok(())
            }
            MouseKind::Click => self.click_at(x, y, code, 1, &mods).await,
            MouseKind::Double => self.click_at(x, y, code, 2, &mods).await,
            MouseKind::Triple => self.click_at(x, y, code, 3, &mods).await,
            MouseKind::RightClick => self.click_at(x, y, 0x111, 1, &mods).await,
            MouseKind::Down => {
                self.portal.pointer_abs(x, y).await?;
                self.note_pointer(x, y);
                self.press_modifiers(&mods, true).await?;
                self.portal.button(code, true).await
            }
            MouseKind::Up => {
                self.portal.pointer_abs(x, y).await?;
                self.note_pointer(x, y);
                let r = self.portal.button(code, false).await;
                self.press_modifiers(&mods, false).await?;
                r
            }
        }
    }

    async fn scroll_at(
        &self,
        x: f64,
        y: f64,
        dir: ScrollDir,
        amount: i32,
    ) -> Result<(), InputError> {
        self.portal.pointer_abs(x, y).await?;
        self.note_pointer(x, y);
        sleep(Duration::from_millis(10)).await;
        let amount = amount.clamp(1, 100);
        let (vertical, sign, steps) = match dir {
            ScrollDir::Up => (true, -1, amount),
            ScrollDir::Down => (true, 1, amount),
            ScrollDir::Left => (false, -1, amount),
            ScrollDir::Right => (false, 1, amount),
            // A page is a burst of wheel steps; the keyboard equivalent would
            // move focus, which a scroll must not do.
            ScrollDir::PageUp => (true, -1, amount * 10),
            ScrollDir::PageDown => (true, 1, amount * 10),
        };
        for _ in 0..steps {
            self.portal.axis_discrete(vertical, sign).await?;
            sleep(Duration::from_millis(4)).await;
        }
        Ok(())
    }

    async fn hover(&self, x: f64, y: f64) -> Result<(), InputError> {
        self.portal.pointer_abs(x, y).await?;
        self.note_pointer(x, y);
        Ok(())
    }

    async fn drag(
        &self,
        from: (f64, f64),
        to: (f64, f64),
        modifiers: &[String],
        steps: u32,
        since_takeover: u64,
    ) -> Result<(), InputError> {
        let mods = keys::pointer_modifiers(modifiers).map_err(InputError::Failed)?;
        self.portal.pointer_abs(from.0, from.1).await?;
        self.note_pointer(from.0, from.1);
        sleep(Duration::from_millis(20)).await;
        self.press_modifiers(&mods, true).await?;
        self.portal.button(0x110, true).await?;
        sleep(Duration::from_millis(30)).await;
        let steps = steps.clamp(1, 200);
        let mut last = from;
        for i in 1..=steps {
            if self.takeover_generation() != since_takeover {
                let _ = self.portal.button(0x110, false).await;
                let _ = self.press_modifiers(&mods, false).await;
                return Err(InputError::Failed(
                    "drag aborted: a human took over the pointer".into(),
                ));
            }
            let t = i as f64 / steps as f64;
            let p = (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t);
            self.portal.pointer_abs(p.0, p.1).await?;
            self.note_pointer(p.0, p.1);
            last = p;
            sleep(Duration::from_millis(8)).await;
        }
        if last != to {
            self.portal.pointer_abs(to.0, to.1).await?;
        }
        sleep(Duration::from_millis(60)).await;
        let r = self.portal.button(0x110, false).await;
        self.press_modifiers(&mods, false).await?;
        r
    }

    async fn clipboard_read(&self, format: ClipFormat) -> Result<ClipData, InputError> {
        let mime = clip::mime_for(format);
        // The Wayland path first; the X11 bridge only if the compositor
        // withholds the protocol, so a machine that has both keeps using
        // the native one.
        let bytes = match wl_read(format).await {
            Ok(b) => b,
            Err(InputError::Failed(m)) if clip::missing_protocol(&m) => {
                x11_read(format, mime).await?
            }
            Err(e) => return Err(e),
        };
        Ok(ClipData {
            format,
            data: bytes.map(|b| encode_clip(format, &b)),
        })
    }

    async fn clipboard_write(&self, format: ClipFormat, data: &str) -> Result<(), InputError> {
        let mime = clip::mime_for(format);
        let bytes = decode_clip(format, data)?;
        match wl_write(format, bytes.clone()).await {
            Err(InputError::Failed(m)) if clip::missing_protocol(&m) => {
                x11_write(format, mime, bytes).await
            }
            other => other,
        }
    }

    /// Where synthetic input will land: the pinned target if it is still on
    /// the bus, else the app with the active window.
    fn input_target(&self) -> Option<String> {
        let target = self.lock().target.clone();
        // This is a synchronous query on an async bus; answer from the last
        // snapshot's knowledge rather than block the runtime.
        target.map(|t| t.name)
    }

    fn platform(&self) -> &'static str {
        "linux"
    }

    /// On X11 the server answers `QueryPointer` (through `xdotool`). Wayland
    /// gives a client no way to ask, and `None` is never treated as evidence
    /// of anything, so the override watcher stays quiet rather than guessing.
    async fn pointer_position(&self) -> Result<Option<(f64, f64)>, InputError> {
        match crate::pointer::support() {
            crate::pointer::PointerSupport::X11 { xdotool } => crate::pointer::query(xdotool)
                .await
                .map_err(InputError::Failed),
            crate::pointer::PointerSupport::Unavailable(_) => Ok(None),
        }
    }

    fn pointer_unavailable_reason(&self) -> Option<String> {
        match crate::pointer::support() {
            crate::pointer::PointerSupport::X11 { .. } => None,
            crate::pointer::PointerSupport::Unavailable(why) => Some(why.clone()),
        }
    }

    fn recent_pointer_sets(&self) -> Vec<SetPoint> {
        self.lock().pointer_sets.clone()
    }

    fn cancel_pending(&self) {
        self.takeovers.fetch_add(1, Ordering::SeqCst);
    }

    fn takeover_generation(&self) -> u64 {
        self.takeovers.load(Ordering::SeqCst)
    }
}

/// How a clipboard payload crosses the JSON boundary: an image is opaque
/// bytes and rides as base64; everything else is text (UTF-8, lossy on the
/// rare non-text byte).
fn encode_clip(format: ClipFormat, bytes: &[u8]) -> String {
    match format {
        ClipFormat::Image => {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode(bytes)
        }
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// The inverse of [`encode_clip`]: an image arrives as base64 and is decoded
/// to the PNG bytes; other formats are UTF-8 already.
fn decode_clip(format: ClipFormat, data: &str) -> Result<Vec<u8>, InputError> {
    match format {
        ClipFormat::Image => {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(data.trim())
                .map_err(|e| {
                    InputError::InvalidArgs(format!(
                        "clipboard image data must be base64-encoded PNG: {e}"
                    ))
                })
        }
        _ => Ok(data.as_bytes().to_vec()),
    }
}

/// The MIME wl-clipboard should ask for. Text stays `MimeType::Text` (which
/// tries the several text spellings a source might advertise); the rest name
/// the exact type.
fn wl_paste_mime(format: ClipFormat) -> wl_clipboard_rs::paste::MimeType<'static> {
    use wl_clipboard_rs::paste::MimeType;
    match format {
        ClipFormat::Text => MimeType::Text,
        _ => MimeType::Specific(clip::mime_for(format)),
    }
}

/// The Wayland clipboard read (`wlr-data-control`). `Failed` carries the raw
/// message so the caller can spot the missing-protocol case. Returns the raw
/// bytes so a PNG survives the trip.
async fn wl_read(format: ClipFormat) -> Result<Option<Vec<u8>>, InputError> {
    use wl_clipboard_rs::paste::{get_contents, ClipboardType, Seat};
    let mime = wl_paste_mime(format);
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        match get_contents(ClipboardType::Regular, Seat::Unspecified, mime) {
            Ok((mut reader, _mime)) => {
                let mut buf = Vec::new();
                reader
                    .read_to_end(&mut buf)
                    .map_err(|e| InputError::Failed(format!("clipboard read: {e}")))?;
                Ok(Some(buf))
            }
            Err(wl_clipboard_rs::paste::Error::NoSeats)
            | Err(wl_clipboard_rs::paste::Error::ClipboardEmpty)
            | Err(wl_clipboard_rs::paste::Error::NoMimeType) => Ok(None),
            Err(e) => Err(InputError::Failed(format!("clipboard read: {e}"))),
        }
    })
    .await
    .map_err(|e| InputError::Failed(format!("clipboard task: {e}")))?
}

async fn wl_write(format: ClipFormat, bytes: Vec<u8>) -> Result<(), InputError> {
    use wl_clipboard_rs::copy::{MimeType, Options, Source};
    let mime = match format {
        ClipFormat::Text => MimeType::Text,
        _ => MimeType::Specific(clip::mime_for(format).into()),
    };
    let bytes: Box<[u8]> = bytes.into();
    tokio::task::spawn_blocking(move || {
        Options::new()
            .copy(Source::Bytes(bytes), mime)
            .map_err(|e| InputError::Failed(format!("clipboard write: {e}")))
    })
    .await
    .map_err(|e| InputError::Failed(format!("clipboard task: {e}")))?
}

/// No data-control protocol: name the limitation and the two ways out.
fn no_clipboard_path() -> InputError {
    InputError::Unsupported(
        "this compositor (GNOME/Mutter) does not implement the wlr-data-control \
         clipboard protocol, and no X11 clipboard tool was found to bridge \
         it; install xclip or xsel, or use a compositor that supports \
         data-control"
            .into(),
    )
}

/// An X11 tool that cannot carry `format` (xsel and a non-text format).
fn x11_cannot_carry(tool: clip::X11Tool, format: ClipFormat) -> InputError {
    InputError::Unsupported(format!(
        "the only X11 clipboard tool found ({tool:?}) carries text only, so it \
         cannot handle {format:?}; install xclip, or use a compositor that \
         implements the wlr-data-control protocol"
    ))
}

/// Read the clipboard through an X11 tool over XWayland. Returns raw bytes so
/// an image survives.
async fn x11_read(format: ClipFormat, mime: &str) -> Result<Option<Vec<u8>>, InputError> {
    let Some((tool, bin)) = clip::X11Tool::detect() else {
        return Err(no_clipboard_path());
    };
    if !tool.supports(format) {
        return Err(x11_cannot_carry(tool, format));
    }
    let out = tokio::process::Command::new(&bin)
        .args(tool.read_args(mime))
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| InputError::Failed(format!("{bin}: {e}")))?;
    if !out.status.success() {
        // An empty clipboard is a non-zero exit for xclip, not a fault.
        return Ok(None);
    }
    Ok(if out.stdout.is_empty() {
        None
    } else {
        Some(out.stdout)
    })
}

/// Write the clipboard through an X11 tool over XWayland.
async fn x11_write(format: ClipFormat, mime: &str, data: Vec<u8>) -> Result<(), InputError> {
    use tokio::io::AsyncWriteExt;
    let Some((tool, bin)) = clip::X11Tool::detect() else {
        return Err(no_clipboard_path());
    };
    if !tool.supports(format) {
        return Err(x11_cannot_carry(tool, format));
    }
    // stdout AND stderr go to /dev/null on purpose. Once xclip has read the
    // data it forks a background process to serve the selection until another
    // client takes it over, and that child inherits any pipe we keep. Capturing
    // stderr would leave the pipe open in the daemon, so waiting for it to close
    // never returns. The parent exits as soon as it has forked, so we wait only
    // on the parent, and bound even that in case a build does not fork.
    let mut child = tokio::process::Command::new(&bin)
        .args(tool.write_args(mime))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| InputError::Failed(format!("{bin}: {e}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(&data)
            .await
            .map_err(|e| InputError::Failed(format!("{bin} stdin: {e}")))?;
        // Closing stdin signals end-of-input; xclip then grabs the selection.
        drop(stdin);
    }
    match tokio::time::timeout(std::time::Duration::from_secs(3), child.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => Err(InputError::Failed(format!("{bin} exited with {status}"))),
        Ok(Err(e)) => Err(InputError::Failed(format!("{bin}: {e}"))),
        // Some xclip builds serve the selection in the foreground rather than
        // forking. It has the selection by now, so leave it running and treat
        // that as success rather than killing it and dropping the clipboard.
        Err(_elapsed) => Ok(()),
    }
}

/// Press a widget through its Action interface: the first action whose name
/// is one of the conventional press verbs, else action 0.
async fn do_press(
    p: &atspi::proxy::accessible::AccessibleProxy<'_>,
    interfaces: atspi::InterfaceSet,
    label: &str,
) -> Result<(), InputError> {
    if !interfaces.contains(Interface::Action) {
        return Err(InputError::Unsupported(format!(
            "'{label}' exposes no actions; it is not something that can be pressed"
        )));
    }
    let a = action_iface(p).await?;
    let actions = a
        .get_actions()
        .await
        .map_err(|e| InputError::Failed(format!("actions: {e}")))?;
    if actions.is_empty() {
        return Err(InputError::Unsupported(format!(
            "'{label}' has an empty action list"
        )));
    }
    let idx = actions
        .iter()
        .position(|act| {
            let n = act.name.to_lowercase();
            a11y::PRESS_ACTIONS.iter().any(|v| n == *v)
        })
        .unwrap_or(0);
    let ok = a
        .do_action(idx as i32)
        .await
        .map_err(|e| InputError::Failed(format!("press: {e}")))?;
    if ok {
        Ok(())
    } else {
        Err(InputError::Failed(format!(
            "'{label}' declined the '{}' action",
            actions[idx].name
        )))
    }
}

type Acc<'a> = atspi::proxy::accessible::AccessibleProxy<'a>;

fn iface_err(what: &str, e: impl std::fmt::Display) -> InputError {
    InputError::Failed(format!("{what}: {e}"))
}

async fn component_iface<'a>(
    p: &Acc<'a>,
) -> Result<atspi::proxy::component::ComponentProxy<'a>, InputError> {
    p.proxies()
        .await
        .map_err(|e| iface_err("proxies", e))?
        .component()
        .await
        .map_err(|e| iface_err("component", e))
}

async fn action_iface<'a>(
    p: &Acc<'a>,
) -> Result<atspi::proxy::action::ActionProxy<'a>, InputError> {
    p.proxies()
        .await
        .map_err(|e| iface_err("proxies", e))?
        .action()
        .await
        .map_err(|e| iface_err("action", e))
}

async fn editable_text_iface<'a>(
    p: &Acc<'a>,
) -> Result<atspi::proxy::editable_text::EditableTextProxy<'a>, InputError> {
    p.proxies()
        .await
        .map_err(|e| iface_err("proxies", e))?
        .editable_text()
        .await
        .map_err(|e| iface_err("editable text", e))
}

async fn text_iface<'a>(p: &Acc<'a>) -> Result<atspi::proxy::text::TextProxy<'a>, InputError> {
    p.proxies()
        .await
        .map_err(|e| iface_err("proxies", e))?
        .text()
        .await
        .map_err(|e| iface_err("text", e))
}

async fn value_iface<'a>(p: &Acc<'a>) -> Result<atspi::proxy::value::ValueProxy<'a>, InputError> {
    p.proxies()
        .await
        .map_err(|e| iface_err("proxies", e))?
        .value()
        .await
        .map_err(|e| iface_err("value", e))
}

async fn selection_iface<'a>(
    p: &Acc<'a>,
) -> Result<atspi::proxy::selection::SelectionProxy<'a>, InputError> {
    p.proxies()
        .await
        .map_err(|e| iface_err("proxies", e))?
        .selection()
        .await
        .map_err(|e| iface_err("selection", e))
}

#[cfg(test)]
mod clip_tests {
    use super::{decode_clip, encode_clip};
    use mcp_input::ClipFormat;

    #[test]
    fn text_html_and_files_round_trip_as_utf8() {
        for f in [ClipFormat::Text, ClipFormat::Html, ClipFormat::Files] {
            let s = "file:///tmp/a\nfile:///tmp/b";
            let bytes = decode_clip(f, s).unwrap();
            assert_eq!(bytes, s.as_bytes());
            assert_eq!(encode_clip(f, &bytes), s);
        }
    }

    #[test]
    fn an_image_round_trips_through_base64_including_non_utf8_bytes() {
        // A PNG signature followed by a byte that is not valid UTF-8 (0xFF),
        // which a text round trip would corrupt.
        let png = [0x89u8, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0xff, 0x00];
        let b64 = encode_clip(ClipFormat::Image, &png);
        assert!(!b64.contains('\u{fffd}'), "base64 is ASCII, never lossy");
        let back = decode_clip(ClipFormat::Image, &b64).unwrap();
        assert_eq!(back, png, "the exact PNG bytes survive the trip");
    }

    #[test]
    fn surrounding_whitespace_on_base64_image_data_is_tolerated() {
        let b64 = encode_clip(ClipFormat::Image, &[1, 2, 3, 4]);
        let padded = format!("  \n{b64}\n ");
        assert_eq!(
            decode_clip(ClipFormat::Image, &padded).unwrap(),
            [1, 2, 3, 4]
        );
    }

    #[test]
    fn malformed_base64_image_data_is_an_invalid_argument() {
        let e = decode_clip(ClipFormat::Image, "not!base64!").unwrap_err();
        assert!(
            matches!(e, mcp_input::InputError::InvalidArgs(_)),
            "expected InvalidArgs, got {e:?}"
        );
    }
}
