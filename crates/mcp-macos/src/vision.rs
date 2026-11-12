//! macOS vision backend: `list_displays` via CGDisplay, screen capture via the
//! `screencapture` CLI (which writes a PNG we read + base64-encode), window
//! capture via ScreenCaptureKit with `screencapture` as the fallback (see
//! `sck.rs`). Capture needs the Screen Recording permission; without it
//! `screencapture` captures are empty or blank and ScreenCaptureKit refuses.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use core_graphics::display::CGDisplay;
use mcp_vision::{
    CaptureOpts, CaptureResult, DisplayInfo, VisionBackend, VisionError, WindowSubject,
};
use mcp_window::WindowError;

use crate::imp::MacosBackend;
use crate::sck;

/// The `via` of a capture the `screencapture` tool made, and of one
/// ScreenCaptureKit made. Set from the path that ran, never from the one tried.
const VIA_SCREENCAPTURE: &str = "screencapture";
const VIA_SCREENCAPTUREKIT: &str = "screencapturekit";

static SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_png() -> PathBuf {
    let mut p = std::env::temp_dir();
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    p.push(format!("agentctl-cap-{}-{n}.png", std::process::id()));
    p
}

/// PNG width/height from the IHDR header.
fn png_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < 24 || &b[0..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let w = u32::from_be_bytes([b[16], b[17], b[18], b[19]]);
    let h = u32::from_be_bytes([b[20], b[21], b[22], b[23]]);
    Some((w, h))
}

/// Standard base64 (no line breaks). Hand-rolled to avoid a dependency.
fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Default longest edge, in pixels, we deliver to the agent.
///
/// Vision models downsample to roughly this anyway, so a full Retina capture
/// (3024x1964 here) costs ~3.7x the image tokens for no extra detail. Resizing
/// is done by `sips`, which ships with macOS — no image-crate dependency.
const MAX_EDGE: u32 = 1568;

/// Downscale in place if the longest edge exceeds `MAX_EDGE`. Best-effort: on
/// any failure the original file is left untouched.
fn downscale(path: &std::path::Path, w: u32, h: u32, max_edge: u32) -> bool {
    if w.max(h) <= max_edge {
        return false;
    }
    std::process::Command::new("/usr/bin/sips")
        .args(["-Z", &max_edge.to_string()])
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Thumbnail edge used for the visual-change signature.
const SIG_EDGE: u32 = 32;

/// Render a tiny thumbnail and return its raw pixel bytes.
///
/// Uses `sips` to emit a BMP (trivially parseable: a 14-byte file header whose
/// offset-to-pixels field we honour) so no image-decoding dependency is needed.
/// Best-effort — an empty vec disables dedup for this frame.
fn signature_of(src: &std::path::Path) -> Vec<u8> {
    let out = src.with_extension("sig.bmp");
    let ok = std::process::Command::new("/usr/bin/sips")
        .args(["-Z", &SIG_EDGE.to_string(), "-s", "format", "bmp", "--out"])
        .arg(&out)
        .arg(src)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        return Vec::new();
    }
    let bytes = std::fs::read(&out).unwrap_or_default();
    let _ = std::fs::remove_file(&out);
    if bytes.len() < 14 {
        return Vec::new();
    }
    let off = u32::from_le_bytes([bytes[10], bytes[11], bytes[12], bytes[13]]) as usize;
    bytes.get(off..).map(<[u8]>::to_vec).unwrap_or_default()
}

/// Capture straight to a PNG file: no downscale, no base64, no thumbnail.
///
/// OCR needs the pixels, and none of the token-cost machinery applies to text
/// that comes back as text.
fn capture_to_file(args: &[String]) -> Result<(std::path::PathBuf, u32, u32), VisionError> {
    let file = temp_png();
    let mut full = args.to_vec();
    full.push(file.to_string_lossy().into_owned());
    let status = std::process::Command::new("/usr/sbin/screencapture")
        .args(&full)
        .status()
        .map_err(|e| VisionError::Failed(format!("screencapture: {e}")))?;
    if !status.success() {
        let _ = std::fs::remove_file(&file);
        return Err(VisionError::Failed("screencapture failed".into()));
    }
    let bytes =
        std::fs::read(&file).map_err(|e| VisionError::Failed(format!("read capture: {e}")))?;
    if bytes.is_empty() {
        let _ = std::fs::remove_file(&file);
        return Err(VisionError::PermissionDenied(
            "capture was empty — grant Screen Recording permission".into(),
        ));
    }
    let (w, h) = png_dimensions(&bytes).unwrap_or((0, 0));
    Ok((file, w, h))
}

fn run_screencapture(args: &[String], max_edge: u32) -> Result<CaptureResult, VisionError> {
    let file = temp_png();
    let mut full = args.to_vec();
    full.push(file.to_string_lossy().into_owned());
    let status = std::process::Command::new("/usr/sbin/screencapture")
        .args(&full)
        .status()
        .map_err(|e| VisionError::Failed(format!("screencapture: {e}")))?;
    if !status.success() {
        let _ = std::fs::remove_file(&file);
        return Err(VisionError::Failed("screencapture failed".into()));
    }
    let bytes =
        std::fs::read(&file).map_err(|e| VisionError::Failed(format!("read capture: {e}")))?;
    if bytes.is_empty() {
        let _ = std::fs::remove_file(&file);
        return Err(VisionError::PermissionDenied(
            "capture was empty — grant Screen Recording permission".into(),
        ));
    }
    let (width, height) = png_dimensions(&bytes).unwrap_or((0, 0));
    // Shrink oversized captures before base64 so the agent is not billed for
    // pixels its model will throw away.
    let (bytes, delivered) = if downscale(&file, width, height, max_edge) {
        match std::fs::read(&file) {
            Ok(b) => {
                let d = png_dimensions(&b).unwrap_or((width, height));
                (b, d)
            }
            Err(_) => (bytes, (width, height)),
        }
    } else {
        (bytes, (width, height))
    };
    let signature = signature_of(&file);
    let _ = std::fs::remove_file(&file);
    Ok(CaptureResult {
        mime_type: "image/png".into(),
        base64: base64_encode(&bytes),
        width: delivered.0,
        height: delivered.1,
        origin: (0.0, 0.0),
        screen_size: (delivered.0 as f64, delivered.1 as f64),
        original: (width, height),
        signature,
        window: None,
        via: VIA_SCREENCAPTURE,
    })
}

impl MacosBackend {
    /// The `screencapture` arguments for a target, and the screen area the
    /// resulting image will cover. Shared by capture and OCR so the two cannot
    /// disagree about what a display index or a region means.
    async fn capture_args(
        &self,
        display: Option<u32>,
        region: Option<(f64, f64, f64, f64)>,
    ) -> Result<(Vec<String>, (f64, f64, f64, f64)), VisionError> {
        let mut args = vec!["-x".to_string(), "-t".to_string(), "png".to_string()];
        if let Some((x, y, w, h)) = region {
            args.push("-R".into());
            args.push(format!(
                "{},{},{},{}",
                x as i64, y as i64, w as i64, h as i64
            ));
            return Ok((args, (x, y, w, h)));
        }
        let displays = VisionBackend::list_displays(self).await?;
        let chosen = match display {
            Some(d) => displays
                .iter()
                .find(|x| x.index == d)
                .ok_or_else(|| VisionError::NotFound(format!("no display with index {d}")))?,
            None => displays
                .iter()
                .find(|d| d.primary)
                .or_else(|| displays.first())
                .ok_or_else(|| VisionError::Failed("no displays".into()))?,
        };
        if let Some(d) = display {
            // screencapture -D is 1-based.
            args.push("-D".into());
            args.push((d + 1).to_string());
        }
        Ok((args, (chosen.x, chosen.y, chosen.w, chosen.h)))
    }

    /// A window's screen rectangle, who it belongs to, and the window
    /// server's id for it. The id is read against the application named, else
    /// the pinned one, else the frontmost; a window that is not drawn is an
    /// error. One that another application's window covers is returned with
    /// who covers it: what the screen shows there is theirs.
    async fn window_region(
        &self,
        window_id: u32,
        app: Option<&str>,
    ) -> Result<ResolvedWindow, VisionError> {
        unsafe {
            let crate::imp::CaptureTarget {
                target,
                info,
                bounds: b,
                number,
                covered,
                occluders,
            } = self
                .window_for_capture(window_id, app)
                .map_err(win_to_vision)?;
            Ok(ResolvedWindow {
                region: (b.x, b.y, b.w, b.h),
                subject: WindowSubject {
                    app: target.name.clone(),
                    pid: info.pid,
                    title: info.title,
                    from: target.from.as_str().to_string(),
                    covered_by: Vec::new(),
                    warning: None,
                },
                window_id,
                number,
                covered,
                occluders,
            })
        }
    }

    /// Capture a resolved window.
    ///
    /// ScreenCaptureKit first, asked for that window's own `CGWindowID`, so
    /// what is in front of it does not matter and no other application's
    /// window can be returned for it. `screencapture` of the window's
    /// rectangle is the fallback where ScreenCaptureKit is missing (macOS
    /// before 14), times out or cannot find the window. The result's `via`
    /// says which path made the image, and the cover warning is attached only
    /// when it was the rectangle grab (see [`geometry::cover_warning_for`]).
    async fn capture_resolved(
        &self,
        w: ResolvedWindow,
        opts: CaptureOpts,
    ) -> Result<CaptureResult, VisionError> {
        let region = w.region;
        let max_edge = opts.max_edge.unwrap_or(MAX_EDGE).clamp(160, 4096);
        let mut fallback_reason = None;
        if let (Some(pid), Ok(id)) = (w.subject.pid, u32::try_from(w.number)) {
            let target = sck::WindowTarget {
                pid: pid as i32,
                window_id: id,
            };
            match sck::capture_window(target, max_edge).await {
                Ok(c) => {
                    return Ok(CaptureResult {
                        mime_type: "image/png".into(),
                        base64: base64_encode(&c.png),
                        width: c.width,
                        height: c.height,
                        origin: (region.0, region.1),
                        screen_size: (region.2, region.3),
                        original: c.original,
                        signature: c.signature,
                        window: Some(self.subject_for(&w, VIA_SCREENCAPTUREKIT)),
                        via: VIA_SCREENCAPTUREKIT,
                    })
                }
                Err(sck::SckFailure::PermissionDenied(m)) => {
                    return Err(VisionError::PermissionDenied(m))
                }
                Err(f) if sck::should_fall_back(&f) => {
                    tracing::debug!(?f, "ScreenCaptureKit unavailable; using screencapture");
                    fallback_reason = Some(f);
                }
                Err(sck::SckFailure::Failed(m)) => return Err(VisionError::Failed(m)),
                Err(f) => return Err(VisionError::Failed(format!("{f:?}"))),
            }
        }
        let mut cap = match (
            self.capture_screen(None, Some(region), opts).await,
            fallback_reason,
        ) {
            (Ok(c), _) => c,
            // Keep the reason the better path was skipped: with a sleeping
            // display both fail, and the first error is the informative one.
            (Err(VisionError::Failed(m)), Some(f)) => {
                return Err(VisionError::Failed(format!(
                    "{m} (ScreenCaptureKit: {f:?})"
                )))
            }
            (Err(e), _) => return Err(e),
        };
        cap.origin = (region.0, region.1);
        cap.screen_size = (region.2, region.3);
        cap.window = Some(self.subject_for(&w, cap.via));
        Ok(cap)
    }

    /// The subject of a capture, with the cover warning the route earns.
    fn subject_for(&self, w: &ResolvedWindow, via: &str) -> WindowSubject {
        let mut s = w.subject.clone();
        if let Some((covered_by, warning)) =
            crate::geometry::cover_warning_for(via, &s.app, w.window_id, &w.covered)
        {
            s.covered_by = covered_by;
            crate::geometry::attach_rects(&mut s.covered_by, &w.occluders);
            s.warning = Some(warning);
        }
        s
    }
}

/// A window resolved for capture or OCR.
struct ResolvedWindow {
    /// Screen rectangle in points: `(x, y, w, h)`.
    region: (f64, f64, f64, f64),
    subject: WindowSubject,
    /// The id the caller gave, for messages.
    window_id: u32,
    /// The window server's id for it: what ScreenCaptureKit is asked for.
    number: i64,
    /// Other applications on top of it, with the share of it each covers.
    covered: Vec<(String, f64)>,
    /// Where those applications' windows sit on this one.
    occluders: Vec<(String, mcp_vision::marks::Rect)>,
}

#[async_trait]
impl VisionBackend for MacosBackend {
    async fn list_displays(&self) -> Result<Vec<DisplayInfo>, VisionError> {
        let ids = CGDisplay::active_displays()
            .map_err(|_| VisionError::Failed("CGGetActiveDisplayList failed".into()))?;
        let main_id = CGDisplay::main().id;
        let mut out = Vec::new();
        for (i, id) in ids.iter().enumerate() {
            let d = CGDisplay::new(*id);
            let b = d.bounds();
            let scale = d
                .display_mode()
                .map(|m| {
                    if m.width() > 0 {
                        m.pixel_width() as f64 / m.width() as f64
                    } else {
                        1.0
                    }
                })
                .unwrap_or(1.0);
            out.push(DisplayInfo {
                index: i as u32,
                id: *id,
                x: b.origin.x,
                y: b.origin.y,
                w: b.size.width,
                h: b.size.height,
                scale,
                primary: *id == main_id,
            });
        }
        Ok(out)
    }

    async fn capture_screen(
        &self,
        display: Option<u32>,
        region: Option<(f64, f64, f64, f64)>,
        opts: CaptureOpts,
    ) -> Result<CaptureResult, VisionError> {
        let max_edge = opts.max_edge.unwrap_or(MAX_EDGE).clamp(160, 4096);
        let (args, area) = self.capture_args(display, region).await?;
        let mut cap = run_screencapture(&args, max_edge)?;
        cap.origin = (area.0, area.1);
        cap.screen_size = (area.2, area.3);
        Ok(cap)
    }

    async fn ocr(
        &self,
        target: mcp_vision::OcrTarget,
        opts: &mcp_vision::OcrOpts,
    ) -> Result<mcp_vision::OcrResult, VisionError> {
        use mcp_vision::OcrTarget;
        let helper = crate::ocr::ensure_helper(&self.helper_dir)?;
        let mut window = None;
        let (args, area) = match target {
            OcrTarget::Display(d) => self.capture_args(d, None).await?,
            OcrTarget::Region(r) => self.capture_args(None, Some(r)).await?,
            OcrTarget::Window { id, app } => {
                let w = self.window_region(id, app.as_deref()).await?;
                // OCR reads the screen, never ScreenCaptureKit, so a covering
                // window's text is in what it reads.
                window = Some(self.subject_for(&w, VIA_SCREENCAPTURE));
                self.capture_args(None, Some(w.region)).await?
            }
        };
        // Full resolution, no downscale and no thumbnail: OCR needs the pixels,
        // and none of the token-cost machinery applies to text that comes back
        // as text.
        let (file, width, height) = capture_to_file(&args)?;
        let result = crate::ocr::ocr_png(&helper, &file, opts).await;
        let _ = std::fs::remove_file(&file);
        let (lines, w, h) = result?;
        Ok(mcp_vision::OcrResult {
            lines,
            width: if w > 0 { w } else { width },
            height: if h > 0 { h } else { height },
            origin: (area.0, area.1),
            screen_size: (area.2, area.3),
            window,
            via: VIA_SCREENCAPTURE,
        })
    }

    async fn capture_window(
        &self,
        window_id: u32,
        app: Option<&str>,
        opts: CaptureOpts,
    ) -> Result<CaptureResult, VisionError> {
        let w = self.window_region(window_id, app).await?;
        self.capture_resolved(w, opts).await
    }

    fn platform(&self) -> &'static str {
        "macos"
    }
}

fn win_to_vision(e: WindowError) -> VisionError {
    match e {
        WindowError::PermissionDenied(m) => VisionError::PermissionDenied(m),
        WindowError::NotFound(m) => VisionError::NotFound(m),
        WindowError::Unsupported(m) => VisionError::Unsupported(m),
        // Only the foreground wrapper refuses, and capture does not go
        // through it; mapped rather than left to fall out of the match.
        WindowError::Failed(m) | WindowError::Refused(m) => VisionError::Failed(m),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions;

    fn skip_live() -> bool {
        std::env::var_os("AGENTCTL_SKIP_LIVE").is_some_and(|v| v != "0")
    }

    /// Windows the machine lists as on screen, with the application they
    /// belong to: `(app, id, covered share)`. Reads only; nothing is raised,
    /// moved or focused.
    async fn onscreen_windows(b: &MacosBackend) -> Vec<(String, u32, f64)> {
        let listed = mcp_window::WindowBackend::list_windows(b, None)
            .await
            .unwrap_or_default();
        let mut out = Vec::new();
        for w in listed {
            let (Some(app), Some(r)) = (w.app.clone(), w.bounds) else {
                continue;
            };
            if w.state != mcp_window::WindowState::Onscreen || r.w < 100.0 || r.h < 100.0 {
                continue;
            }
            if let Ok(t) = unsafe { b.window_for_capture(w.id, Some(&app)) } {
                let share: f64 = t.covered.iter().map(|(_, s)| s).sum();
                out.push((app, w.id, share));
            }
        }
        out
    }

    /// Save a capture's PNG where a person can look at it, when asked to.
    fn save_png(cap: &CaptureResult) {
        let Some(path) = std::env::var_os("AGENTCTL_SAVE_CAPTURE") else {
            return;
        };
        use std::io::Write;
        let mut child = std::process::Command::new("/usr/bin/base64")
            .args(["-D", "-o"])
            .arg(&path)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("base64");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(cap.base64.as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success());
        eprintln!("saved capture to {}", std::path::Path::new(&path).display());
    }

    /// What a window capture of a real window must satisfy, whichever path
    /// made it. Returns the capture when there was one.
    async fn check_capture(
        b: &MacosBackend,
        app: &str,
        id: u32,
        covered: f64,
    ) -> Option<CaptureResult> {
        let opts = CaptureOpts {
            max_edge: Some(4096),
        };
        match VisionBackend::capture_window(b, id, Some(app), opts).await {
            Ok(cap) => {
                eprintln!(
                    "{app} window {id}: via {}, {}x{} px (native {}x{}), covered {:.0}%, warning: {:?}, covered_by: {:?}",
                    cap.via,
                    cap.width,
                    cap.height,
                    cap.original.0,
                    cap.original.1,
                    covered * 100.0,
                    cap.window.as_ref().and_then(|w| w.warning.clone()),
                    cap.window.as_ref().map(|w| &w.covered_by),
                );
                assert!(
                    ["screencapturekit", "screencapture"].contains(&cap.via),
                    "{}",
                    cap.via
                );
                let w = cap
                    .window
                    .as_ref()
                    .expect("a window capture names its window");
                assert_eq!(w.app, app);
                // The pixels are the window's own on ScreenCaptureKit, so
                // there is nothing to warn about; on the screen grab a cover of
                // 5% or more must be said.
                if cap.via == "screencapturekit" {
                    assert!(w.warning.is_none() && w.covered_by.is_empty());
                } else if covered >= crate::geometry::MAX_COVERED_SHARE {
                    assert!(w.warning.is_some() && !w.covered_by.is_empty(), "{w:?}");
                }
                let (pw, ph, rgba) =
                    mcp_vision::grid::decode_rgba_b64(&cap.base64).expect("PNG decodes");
                assert_eq!((pw, ph), (cap.width, cap.height));
                assert!(
                    rgba.chunks_exact(4).any(|p| p[3] != 0),
                    "a capture reported as success must not be blank"
                );
                Some(cap)
            }
            Err(VisionError::PermissionDenied(m)) => {
                eprintln!("Screen Recording not granted: {m}");
                None
            }
            Err(VisionError::Unsupported(m)) => {
                eprintln!("unsupported: {m}");
                None
            }
            // A sleeping display cannot be captured by either path; that is
            // an honest error and waking the screen is not this test's job.
            Err(e) if CGDisplay::main().is_asleep() => {
                eprintln!("display is asleep; capture reported: {e:?}");
                None
            }
            Err(e) => panic!("capture of {app} window {id} failed: {e:?}"),
        }
    }

    /// A window that is on screen is captured at its size times the display
    /// scale, with the mapping other tools rely on. Passive: it takes an
    /// image and raises, moves and focuses nothing, so it is gated by
    /// `AGENTCTL_SKIP_LIVE` only. Without a permission the matching error is
    /// the accepted answer, never a faked image.
    #[tokio::test]
    async fn capturing_a_real_window_returns_its_own_pixels_at_display_scale() {
        if skip_live() {
            eprintln!("skipping: AGENTCTL_SKIP_LIVE is set");
            return;
        }
        let b = MacosBackend::new();
        let wins = onscreen_windows(&b).await;
        // ScreenCaptureKit cannot start a stream for every on-screen window
        // (error -3811 was seen for some), and those fall back, which is a
        // documented answer; so up to six windows are tried, and with the
        // permission, the API and an awake display at least one of them must
        // have come through ScreenCaptureKit.
        let mut sorted = wins.clone();
        sorted.sort_by(|a, b| a.2.total_cmp(&b.2));
        let mut first = None;
        let mut via_sck = false;
        for (app, id, covered) in sorted.into_iter().take(6) {
            if let Some(cap) = check_capture(&b, &app, id, covered).await {
                via_sck |= cap.via == "screencapturekit";
                first.get_or_insert((app, id, cap));
            }
        }
        let Some((app, id, cap)) = first else {
            eprintln!("skipping: nothing could be captured (see the errors above)");
            return;
        };
        if permissions().screen_recording
            && sck::screenshot_api_available()
            && !CGDisplay::main().is_asleep()
        {
            assert!(via_sck, "no window came through ScreenCaptureKit");
        }
        let t = unsafe { b.window_for_capture(id, Some(&app)) }.expect("still there");
        let displays = VisionBackend::list_displays(&b).await.expect("displays");
        let scale = displays
            .iter()
            .find(|d| {
                t.bounds.x >= d.x
                    && t.bounds.x < d.x + d.w
                    && t.bounds.y >= d.y
                    && t.bounds.y < d.y + d.h
            })
            .or_else(|| displays.iter().find(|d| d.primary))
            .map_or(1.0, |d| d.scale);
        let want = (t.bounds.w * scale, t.bounds.h * scale);
        let slack = 4.0 * scale;
        assert!(
            (cap.original.0 as f64 - want.0).abs() <= slack
                && (cap.original.1 as f64 - want.1).abs() <= slack,
            "{}x{} px for a {}x{} pt window at scale {scale}",
            cap.original.0,
            cap.original.1,
            t.bounds.w,
            t.bounds.h
        );
        assert_eq!(cap.screen_size, (t.bounds.w, t.bounds.h));
        assert!((cap.scale_x() * cap.width as f64 - t.bounds.w).abs() < 1e-6);
    }

    /// The case ScreenCaptureKit exists for: a window that something else
    /// covers. Whatever route answers, the result says honestly whose pixels
    /// they are. Saves the image when `AGENTCTL_SAVE_CAPTURE` names a path.
    #[tokio::test]
    async fn a_covered_window_is_captured_and_the_route_decides_the_warning() {
        if skip_live() {
            eprintln!("skipping: AGENTCTL_SKIP_LIVE is set");
            return;
        }
        let b = MacosBackend::new();
        let wins = onscreen_windows(&b).await;
        let Some((app, id, covered)) = wins
            .iter()
            .filter(|w| w.2 >= crate::geometry::MAX_COVERED_SHARE)
            // `AGENTCTL_CAPTURE_APP` picks the application, so a saved image
            // is of one the person chose to share.
            .filter(|w| {
                std::env::var("AGENTCTL_CAPTURE_APP").map_or(true, |a| a.eq_ignore_ascii_case(&w.0))
            })
            .max_by(|a, b| a.2.total_cmp(&b.2))
            .cloned()
        else {
            eprintln!("skipping: no on-screen window is covered by another app's right now");
            return;
        };
        if let Some(cap) = check_capture(&b, &app, id, covered).await {
            save_png(&cap);
        }
    }
}
