//! Displays, capture and text recognition.
//!
//! Capture is the `Screenshot` portal: one D-Bus call, a PNG on disk, which
//! is read, cropped, downscaled and deleted. The first call on a fresh
//! session may take several seconds while the portal records the grant;
//! after that it is about half a second on GNOME. Display geometry comes
//! from Mutter's `DisplayConfig`, the only place logical monitor layout is
//! published; on another compositor the screenshot's own size stands in for
//! it, as one display.
//!
//! Text recognition is the `ocrs` engine, run in-process on the CPU. Its
//! two models are downloaded on first use into the agentctl state directory,
//! the way the macOS backend compiles its helper on first use, and can be
//! placed there ahead of time on a machine that must not fetch anything.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mcp_vision::{
    CaptureOpts, CaptureResult, DisplayInfo, OcrLine, OcrOpts, OcrResult, OcrTarget, VisionBackend,
    VisionError,
};

use crate::backend::LinuxBackend;
use crate::image::{decode_png, Rgb};

/// Default longest edge delivered to the agent, matching macOS.
const MAX_EDGE: u32 = 1568;

fn fail(e: impl std::fmt::Display) -> VisionError {
    VisionError::Failed(e.to_string())
}

/// A logical monitor as Mutter reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct Monitor {
    pub connector: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub scale: f64,
    pub primary: bool,
}

type MonitorSpec = (
    (String, String, String, String),
    Vec<(
        String,
        i32,
        i32,
        f64,
        f64,
        Vec<f64>,
        std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    )>,
    std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
);
type LogicalSpec = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<(String, String, String, String)>,
    std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
);
type CurrentState = (
    u32,
    Vec<MonitorSpec>,
    Vec<LogicalSpec>,
    std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
);

/// Turn Mutter's `GetCurrentState` reply into monitors. Pure, so the shape
/// can be tested without a compositor.
pub fn monitors_from_state(state: &CurrentState) -> Vec<Monitor> {
    let (_, monitors, logical, _) = state;
    let mut out = Vec::new();
    for (x, y, scale, transform, primary, links, _) in logical {
        let Some((connector, _, _, _)) = links.first() else {
            continue;
        };
        // Size comes from the current mode of the physical monitor, scaled
        // and rotated.
        let Some(spec) = monitors.iter().find(|m| m.0 .0 == *connector) else {
            continue;
        };
        let current = spec.1.iter().find(|m| {
            m.6.get("is-current")
                .and_then(|v| bool::try_from(v.clone()).ok())
                .unwrap_or(false)
        });
        let Some(mode) = current.or(spec.1.first()) else {
            continue;
        };
        let (mut w, mut h) = (mode.1, mode.2);
        if transform % 2 == 1 {
            std::mem::swap(&mut w, &mut h);
        }
        let s = if *scale > 0.0 { *scale } else { 1.0 };
        out.push(Monitor {
            connector: connector.clone(),
            x: *x,
            y: *y,
            w: (w as f64 / s).round() as i32,
            h: (h as f64 / s).round() as i32,
            scale: s,
            primary: *primary,
        });
    }
    out.sort_by_key(|m| (!m.primary, m.x, m.y));
    out
}

/// The monitors Mutter reports, for the desktop engine's `resolution` read.
pub async fn monitors_public() -> Result<Vec<Monitor>, String> {
    mutter_monitors().await
}

async fn mutter_monitors() -> Result<Vec<Monitor>, String> {
    let conn = zbus::Connection::session()
        .await
        .map_err(|e| e.to_string())?;
    let proxy = zbus::Proxy::new(
        &conn,
        "org.gnome.Mutter.DisplayConfig",
        "/org/gnome/Mutter/DisplayConfig",
        "org.gnome.Mutter.DisplayConfig",
    )
    .await
    .map_err(|e| e.to_string())?;
    let state: CurrentState = proxy
        .call("GetCurrentState", &())
        .await
        .map_err(|e| format!("Mutter DisplayConfig: {e}"))?;
    Ok(monitors_from_state(&state))
}

/// Take a screenshot through the portal and return the decoded pixels. The
/// file the portal wrote is removed once read: it is ours, and leaving one
/// per call in the Pictures folder is not a feature.
async fn portal_screenshot() -> Result<Rgb, VisionError> {
    use ashpd::desktop::screenshot::Screenshot;
    let request = tokio::time::timeout(
        Duration::from_secs(30),
        Screenshot::request().interactive(false).modal(false).send(),
    )
    .await
    .map_err(|_| VisionError::Failed("the screenshot portal did not answer within 30s".into()))?
    .map_err(|e| VisionError::Unsupported(format!("no Screenshot portal on this session: {e}")))?;
    let shot = match request.response() {
        Ok(s) => s,
        Err(ashpd::Error::Response(r)) => {
            return Err(VisionError::PermissionDenied(format!(
                "screen capture was not allowed by the portal ({r:?})"
            )))
        }
        Err(e) => return Err(fail(format!("screenshot portal: {e}"))),
    };
    let uri = shot.uri().clone();
    let path = uri
        .to_file_path()
        .map_err(|_| fail(format!("screenshot portal returned a non-file URI: {uri}")))?;
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| fail(format!("reading {}: {e}", path.display())))?;
    if let Err(e) = tokio::fs::remove_file(&path).await {
        tracing::warn!(path = %path.display(), error = %e, "could not remove the portal's screenshot file");
    }
    decode_png(&bytes).map_err(VisionError::Failed)
}

/// Full-desktop geometry: the logical bounding box of every monitor, plus the
/// pixel size of the capture, so a point on the image maps back to a logical
/// coordinate even with fractional scaling.
struct Desk {
    monitors: Vec<Monitor>,
    /// Logical origin and size of the whole desktop.
    origin: (f64, f64),
    size: (f64, f64),
}

impl Desk {
    fn from_monitors(monitors: Vec<Monitor>, px: (u32, u32)) -> Desk {
        if monitors.is_empty() {
            return Desk {
                monitors,
                origin: (0.0, 0.0),
                size: (px.0 as f64, px.1 as f64),
            };
        }
        let x0 = monitors.iter().map(|m| m.x).min().unwrap_or(0) as f64;
        let y0 = monitors.iter().map(|m| m.y).min().unwrap_or(0) as f64;
        let x1 = monitors.iter().map(|m| m.x + m.w).max().unwrap_or(0) as f64;
        let y1 = monitors.iter().map(|m| m.y + m.h).max().unwrap_or(0) as f64;
        Desk {
            monitors,
            origin: (x0, y0),
            size: ((x1 - x0).max(1.0), (y1 - y0).max(1.0)),
        }
    }

    /// Pixels per logical unit of the capture.
    fn px_per_unit(&self, px: (u32, u32)) -> (f64, f64) {
        (px.0 as f64 / self.size.0, px.1 as f64 / self.size.1)
    }
}

impl LinuxBackend {
    async fn desk(&self, px: (u32, u32)) -> Desk {
        let monitors = match mutter_monitors().await {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(error = %e, "no Mutter display geometry; treating the capture as one display");
                Vec::new()
            }
        };
        Desk::from_monitors(monitors, px)
    }

    /// Capture, then crop to a logical region (or a display), keeping the
    /// mapping back to screen coordinates.
    async fn capture_region(
        &self,
        display: Option<u32>,
        region: Option<(f64, f64, f64, f64)>,
    ) -> Result<(Rgb, (f64, f64), (f64, f64)), VisionError> {
        let full = portal_screenshot().await?;
        let desk = self.desk((full.width, full.height)).await;
        let (sx, sy) = desk.px_per_unit((full.width, full.height));
        let (ox, oy, w, h) = match (region, display) {
            (Some(r), _) => r,
            (None, Some(idx)) => {
                let m = desk.monitors.get(idx as usize).ok_or_else(|| {
                    VisionError::NotFound(format!(
                        "no display {idx}; list_displays reports {}",
                        desk.monitors.len()
                    ))
                })?;
                (m.x as f64, m.y as f64, m.w as f64, m.h as f64)
            }
            (None, None) => (desk.origin.0, desk.origin.1, desk.size.0, desk.size.1),
        };
        if region.is_none() && display.is_none() {
            return Ok((full, desk.origin, desk.size));
        }
        let px = full
            .crop(
                ((ox - desk.origin.0) * sx).round() as i64,
                ((oy - desk.origin.1) * sy).round() as i64,
                (w * sx).round() as i64,
                (h * sy).round() as i64,
            )
            .map_err(VisionError::Failed)?;
        Ok((px, (ox, oy), (w, h)))
    }
}

fn deliver(
    img: Rgb,
    origin: (f64, f64),
    screen: (f64, f64),
    max_edge: u32,
) -> Result<CaptureResult, VisionError> {
    let original = (img.width, img.height);
    let small = img.fit(max_edge);
    Ok(CaptureResult {
        mime_type: "image/png".into(),
        base64: small.to_png_base64().map_err(VisionError::Failed)?,
        width: small.width,
        height: small.height,
        origin,
        screen_size: screen,
        original,
        signature: img.signature(),
    })
}

#[async_trait]
impl VisionBackend for LinuxBackend {
    async fn list_displays(&self) -> Result<Vec<DisplayInfo>, VisionError> {
        match mutter_monitors().await {
            Ok(ms) if !ms.is_empty() => Ok(ms
                .iter()
                .enumerate()
                .map(|(i, m)| DisplayInfo {
                    index: i as u32,
                    id: i as u32,
                    x: m.x as f64,
                    y: m.y as f64,
                    w: m.w as f64,
                    h: m.h as f64,
                    scale: m.scale,
                    primary: m.primary,
                })
                .collect()),
            Ok(_) | Err(_) => {
                // Not GNOME: the desktop is one display the size of a capture.
                let shot = portal_screenshot().await?;
                Ok(vec![DisplayInfo {
                    index: 0,
                    id: 0,
                    x: 0.0,
                    y: 0.0,
                    w: shot.width as f64,
                    h: shot.height as f64,
                    scale: 1.0,
                    primary: true,
                }])
            }
        }
    }

    async fn capture_screen(
        &self,
        display: Option<u32>,
        region: Option<(f64, f64, f64, f64)>,
        opts: CaptureOpts,
    ) -> Result<CaptureResult, VisionError> {
        let (img, origin, screen) = self.capture_region(display, region).await?;
        deliver(img, origin, screen, opts.max_edge.unwrap_or(MAX_EDGE))
    }

    async fn capture_window(
        &self,
        window_id: u32,
        opts: CaptureOpts,
    ) -> Result<CaptureResult, VisionError> {
        let win = self.window_by_id(window_id).await.map_err(|e| match e {
            mcp_window::WindowError::NotFound(m) => VisionError::NotFound(m),
            mcp_window::WindowError::PermissionDenied(m) => VisionError::PermissionDenied(m),
            mcp_window::WindowError::Unsupported(m) => VisionError::Unsupported(m),
            mcp_window::WindowError::Failed(m) => VisionError::Failed(m),
        })?;
        let b = win.bounds.ok_or_else(|| {
            VisionError::Unsupported(format!("window '{}' reports no bounds", win.title))
        })?;
        // A Wayland-native toolkit reports its window at (0, 0) because the
        // compositor never tells it where it is. Cropping there would return
        // the top-left corner of the desktop and call it the window.
        if b.x == 0.0 && b.y == 0.0 {
            return Err(VisionError::Unsupported(format!(
                "'{}' does not report a screen position (Wayland hides window placement from applications), so it cannot be cropped out of the desktop; use capture_screen, or ocr_region on the whole screen",
                win.title
            )));
        }
        let (img, origin, screen) = self
            .capture_region(None, Some((b.x, b.y, b.w, b.h)))
            .await?;
        deliver(img, origin, screen, opts.max_edge.unwrap_or(MAX_EDGE))
    }

    fn platform(&self) -> &'static str {
        "linux"
    }

    async fn ocr(&self, target: OcrTarget, opts: &OcrOpts) -> Result<OcrResult, VisionError> {
        let (img, origin, screen) = match target {
            OcrTarget::Display(d) => self.capture_region(d, None).await?,
            OcrTarget::Region(r) => self.capture_region(None, Some(r)).await?,
            OcrTarget::Window(id) => {
                let win = self
                    .window_by_id(id)
                    .await
                    .map_err(|e| VisionError::Failed(e_str(e)))?;
                let b = win
                    .bounds
                    .filter(|b| b.x != 0.0 || b.y != 0.0)
                    .ok_or_else(|| {
                        VisionError::Unsupported(format!(
                            "'{}' reports no screen position; read the whole display instead",
                            win.title
                        ))
                    })?;
                self.capture_region(None, Some((b.x, b.y, b.w, b.h)))
                    .await?
            }
        };
        let engine = self
            .ocr
            .get_or_try_init(|| async {
                let dir = Ocr::model_dir(&self.helper_dir);
                Ocr::load(&dir).await.map(Arc::new)
            })
            .await
            .map_err(|e| VisionError::Failed(e.clone()))?
            .clone();
        let (w, h) = (img.width, img.height);
        let min_conf = opts.min_confidence;
        let lines = tokio::task::spawn_blocking(move || engine.recognise(&img))
            .await
            .map_err(|e| fail(format!("ocr task: {e}")))?
            .map_err(VisionError::Failed)?;
        Ok(OcrResult {
            lines: lines
                .into_iter()
                .filter(|l| l.confidence >= min_conf)
                .collect(),
            width: w,
            height: h,
            origin,
            screen_size: screen,
        })
    }
}

fn e_str(e: mcp_window::WindowError) -> String {
    match e {
        mcp_window::WindowError::PermissionDenied(m)
        | mcp_window::WindowError::NotFound(m)
        | mcp_window::WindowError::Unsupported(m)
        | mcp_window::WindowError::Failed(m) => m,
    }
}

/// Where the models come from, and what they are called on disk.
pub const MODEL_URLS: [(&str, &str); 2] = [
    (
        "text-detection.onnx",
        "https://ocrs-models.s3-accelerate.amazonaws.com/text-detection.onnx",
    ),
    (
        "text-recognition.onnx",
        "https://ocrs-models.s3-accelerate.amazonaws.com/text-recognition.onnx",
    ),
];

/// The loaded recogniser.
pub struct Ocr {
    engine: ocrs::OcrEngine,
}

impl Ocr {
    /// Where the models live: `$AGENTCTL_OCR_MODELS` when set (a shared or
    /// pre-populated directory), else `<state>/ocr`.
    pub fn model_dir(state_dir: &Path) -> PathBuf {
        std::env::var_os("AGENTCTL_OCR_MODELS")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| state_dir.join("ocr"))
    }

    /// Both model files, present or fetched.
    pub fn model_paths(dir: &Path) -> [PathBuf; 2] {
        [dir.join(MODEL_URLS[0].0), dir.join(MODEL_URLS[1].0)]
    }

    pub fn models_present(dir: &Path) -> bool {
        Self::model_paths(dir).iter().all(|p| p.is_file())
    }

    async fn fetch(dir: &Path) -> Result<(), String> {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| format!("creating {}: {e}", dir.display()))?;
        for (name, url) in MODEL_URLS {
            let dest = dir.join(name);
            if dest.is_file() {
                continue;
            }
            tracing::info!(url, dest = %dest.display(), "downloading OCR model (first use)");
            let tmp = dir.join(format!("{name}.part"));
            let out = tokio::process::Command::new("/usr/bin/curl")
                .args(["-fsSL", "--max-time", "300", "-o"])
                .arg(&tmp)
                .arg(url)
                .output()
                .await
                .map_err(|e| {
                    format!(
                        "curl: {e} (place {name} in {} by hand to skip the download)",
                        dir.display()
                    )
                })?;
            if !out.status.success() {
                let _ = tokio::fs::remove_file(&tmp).await;
                return Err(format!(
                    "downloading {url} failed: {} (place {name} in {} by hand to skip the download)",
                    String::from_utf8_lossy(&out.stderr).trim(),
                    dir.display()
                ));
            }
            tokio::fs::rename(&tmp, &dest)
                .await
                .map_err(|e| format!("moving {}: {e}", tmp.display()))?;
        }
        Ok(())
    }

    pub async fn load(dir: &Path) -> Result<Ocr, String> {
        if !Self::models_present(dir) {
            Self::fetch(dir).await?;
        }
        let [det, rec] = Self::model_paths(dir);
        let dir = dir.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let detection_model = rten::Model::load_file(&det).map_err(|e| {
                format!("loading {}: {e} (delete it to re-download)", det.display())
            })?;
            let recognition_model = rten::Model::load_file(&rec).map_err(|e| {
                format!("loading {}: {e} (delete it to re-download)", rec.display())
            })?;
            let engine = ocrs::OcrEngine::new(ocrs::OcrEngineParams {
                detection_model: Some(detection_model),
                recognition_model: Some(recognition_model),
                ..Default::default()
            })
            .map_err(|e| format!("ocr engine: {e}"))?;
            tracing::info!(dir = %dir.display(), "OCR models loaded");
            Ok(Ocr { engine })
        })
        .await
        .map_err(|e| format!("ocr load task: {e}"))?
    }

    /// Recognise text lines in an RGB image. Boxes are image pixels.
    ///
    /// `ocrs` reports no per-line probability, so `confidence` is 1.0 for
    /// every line it returns; a `min_confidence` above that filters
    /// everything, which is the honest outcome of asking for a number the
    /// engine does not have.
    pub fn recognise(&self, img: &Rgb) -> Result<Vec<OcrLine>, String> {
        use ocrs::{ImageSource, TextItem};
        let source = ImageSource::from_bytes(&img.data, (img.width, img.height))
            .map_err(|e| format!("ocr input: {e}"))?;
        let input = self
            .engine
            .prepare_input(source)
            .map_err(|e| format!("ocr prepare: {e}"))?;
        let words = self
            .engine
            .detect_words(&input)
            .map_err(|e| format!("ocr detect: {e}"))?;
        let lines = self.engine.find_text_lines(&input, &words);
        let texts = self
            .engine
            .recognize_text(&input, &lines)
            .map_err(|e| format!("ocr recognise: {e}"))?;
        let mut out = Vec::new();
        for line in texts.into_iter().flatten() {
            let text = line.to_string();
            if text.trim().is_empty() {
                continue;
            }
            let chars = line.chars();
            let left = chars.iter().map(|c| c.rect.left()).min().unwrap_or(0);
            let top = chars.iter().map(|c| c.rect.top()).min().unwrap_or(0);
            let right = chars.iter().map(|c| c.rect.right()).max().unwrap_or(0);
            let bottom = chars.iter().map(|c| c.rect.bottom()).max().unwrap_or(0);
            out.push(OcrLine {
                text,
                confidence: 1.0,
                px: (
                    left as f64,
                    top as f64,
                    (right - left).max(1) as f64,
                    (bottom - top).max(1) as f64,
                ),
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use zbus::zvariant::{OwnedValue, Value};

    fn state(
        logical: Vec<(i32, i32, f64, u32, bool, &str)>,
        modes: Vec<(&str, i32, i32, bool)>,
    ) -> CurrentState {
        let mut monitors: Vec<MonitorSpec> = Vec::new();
        for (conn, w, h, current) in modes {
            let mut props: HashMap<String, OwnedValue> = HashMap::new();
            if current {
                props.insert(
                    "is-current".into(),
                    OwnedValue::try_from(Value::Bool(true)).unwrap(),
                );
            }
            let key = (conn.to_string(), "V".into(), "P".into(), "S".into());
            match monitors.iter_mut().find(|m| m.0 .0 == conn) {
                Some(m) => m.1.push(("m".into(), w, h, 60.0, 1.0, vec![1.0], props)),
                None => monitors.push((
                    key,
                    vec![("m".into(), w, h, 60.0, 1.0, vec![1.0], props)],
                    HashMap::new(),
                )),
            }
        }
        let logical = logical
            .into_iter()
            .map(|(x, y, s, t, p, c)| {
                (
                    x,
                    y,
                    s,
                    t,
                    p,
                    vec![(c.to_string(), "V".into(), "P".into(), "S".into())],
                    HashMap::new(),
                )
            })
            .collect();
        (1, monitors, logical, HashMap::new())
    }

    #[test]
    fn monitors_come_from_the_current_mode_scaled_and_rotated() {
        let st = state(
            vec![
                (0, 0, 1.0, 0, true, "HDMI-1"),
                (1920, 0, 2.0, 1, false, "DP-1"),
            ],
            vec![
                ("HDMI-1", 1920, 1080, false),
                ("HDMI-1", 1280, 720, true),
                ("DP-1", 3840, 2160, true),
            ],
        );
        let ms = monitors_from_state(&st);
        assert_eq!(ms.len(), 2);
        assert_eq!(
            (ms[0].connector.as_str(), ms[0].w, ms[0].h, ms[0].primary),
            ("HDMI-1", 1280, 720, true)
        );
        // Scale 2 halves the logical size; transform 1 rotates 90 degrees.
        assert_eq!(
            (ms[1].x, ms[1].w, ms[1].h, ms[1].scale),
            (1920, 1080, 1920, 2.0)
        );
    }

    #[test]
    fn empty_and_dangling_states_produce_no_monitors() {
        assert!(monitors_from_state(&state(vec![], vec![])).is_empty());
        // A logical monitor pointing at a connector with no modes is skipped.
        let st = state(
            vec![(0, 0, 1.0, 0, true, "GHOST")],
            vec![("HDMI-1", 1920, 1080, true)],
        );
        assert!(monitors_from_state(&st).is_empty());
        // Zero scale is treated as 1 rather than dividing by it.
        let st = state(
            vec![(0, 0, 0.0, 0, true, "HDMI-1")],
            vec![("HDMI-1", 1920, 1080, true)],
        );
        assert_eq!(monitors_from_state(&st)[0].w, 1920);
    }

    #[test]
    fn desk_geometry_is_the_union_of_monitors_or_the_capture() {
        let d = Desk::from_monitors(vec![], (800, 600));
        assert_eq!((d.origin, d.size), ((0.0, 0.0), (800.0, 600.0)));
        let d = Desk::from_monitors(
            vec![
                Monitor {
                    connector: "a".into(),
                    x: 0,
                    y: 0,
                    w: 1920,
                    h: 1080,
                    scale: 1.0,
                    primary: true,
                },
                Monitor {
                    connector: "b".into(),
                    x: 1920,
                    y: 0,
                    w: 1920,
                    h: 1080,
                    scale: 1.0,
                    primary: false,
                },
            ],
            (3840, 1080),
        );
        assert_eq!(d.size, (3840.0, 1080.0));
        assert_eq!(d.px_per_unit((3840, 1080)), (1.0, 1.0));
        // Fractional scaling: 2x pixels per logical unit.
        assert_eq!(d.px_per_unit((7680, 2160)), (2.0, 2.0));
    }

    #[test]
    fn delivered_captures_carry_the_mapping_back_to_screen_space() {
        let img = Rgb::new(4000, 2000, vec![7; 4000 * 2000 * 3]).unwrap();
        let cap = deliver(img, (100.0, 50.0), (2000.0, 1000.0), 1000).unwrap();
        assert_eq!((cap.width, cap.height), (1000, 500));
        assert_eq!(cap.original, (4000, 2000));
        assert_eq!(cap.origin, (100.0, 50.0));
        assert!((cap.scale_x() - 2.0).abs() < 1e-9);
        assert_eq!(cap.signature.len(), 32 * 16 * 3);
        assert!(cap.base64.starts_with("iVBOR"));
    }

    #[test]
    fn model_paths_are_stable_and_absent_by_default() {
        let dir = std::env::temp_dir().join(format!("agentctl-ocr-{}", std::process::id()));
        assert!(!Ocr::models_present(&dir));
        // Without the override the models sit under the state directory.
        if std::env::var_os("AGENTCTL_OCR_MODELS").is_none() {
            assert_eq!(Ocr::model_dir(&dir), dir.join("ocr"));
        }
        let [a, b] = Ocr::model_paths(&dir);
        assert!(a.ends_with("text-detection.onnx"));
        assert!(b.ends_with("text-recognition.onnx"));
    }
}
