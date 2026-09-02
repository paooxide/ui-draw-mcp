//! macOS vision backend: `list_displays` via CGDisplay, screen/window capture
//! via the `screencapture` CLI (which writes a PNG we read + base64-encode).
//! Capture needs the Screen Recording permission; without it captures are empty
//! or blank.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use core_foundation::base::TCFType;
use core_graphics::display::CGDisplay;
use mcp_vision::{CaptureOpts, CaptureResult, DisplayInfo, VisionBackend, VisionError};
use mcp_window::WindowError;

use crate::imp::{focused_app_element, get_windows, read_bounds, MacosBackend};

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
    })
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
        let mut args = vec!["-x".to_string(), "-t".to_string(), "png".to_string()];
        // The screen area the image will cover, in logical coordinates — this
        // is what makes image pixels mappable back to `mouse_action` points.
        let area: (f64, f64, f64, f64);
        if let Some((x, y, w, h)) = region {
            args.push("-R".into());
            args.push(format!(
                "{},{},{},{}",
                x as i64, y as i64, w as i64, h as i64
            ));
            area = (x, y, w, h);
        } else {
            let displays = self.list_displays().await?;
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
            area = (chosen.x, chosen.y, chosen.w, chosen.h);
        }
        let mut cap = run_screencapture(&args, max_edge)?;
        cap.origin = (area.0, area.1);
        cap.screen_size = (area.2, area.3);
        Ok(cap)
    }

    async fn capture_window(
        &self,
        window_id: u32,
        opts: CaptureOpts,
    ) -> Result<CaptureResult, VisionError> {
        // Resolve the AX window's bounds and capture that region.
        let region = unsafe {
            let app = focused_app_element().map_err(win_to_vision)?;
            let wins = get_windows(app.as_CFTypeRef());
            let w = wins
                .get(window_id as usize)
                .ok_or_else(|| VisionError::NotFound(format!("window {window_id} not found")))?;
            read_bounds(w.as_CFTypeRef())
                .map(|b| (b.x, b.y, b.w, b.h))
                .ok_or_else(|| VisionError::Failed("window has no bounds".into()))?
        };
        let mut cap = self.capture_screen(None, Some(region), opts).await?;
        cap.origin = (region.0, region.1);
        cap.screen_size = (region.2, region.3);
        Ok(cap)
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
        WindowError::Failed(m) => VisionError::Failed(m),
    }
}
