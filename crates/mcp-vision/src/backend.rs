use async_trait::async_trait;
use serde::Serialize;

/// A display/monitor.
#[derive(Debug, Clone, Serialize)]
pub struct DisplayInfo {
    pub index: u32,
    pub id: u32,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub scale: f64,
    pub primary: bool,
}

/// A captured image (PNG, base64), plus everything an agent needs to map a
/// point it read off the image back to a screen coordinate for `mouse_action`.
///
/// This matters because the two spaces differ twice over: a Retina capture is
/// already 2x the logical coordinate space, and we then downscale for token
/// cost. Reading a pixel off the image and passing it straight to `mouse_action`
/// would land in the wrong place, so the mapping is reported explicitly.
#[derive(Debug, Clone)]
pub struct CaptureResult {
    pub mime_type: String,
    pub base64: String,
    /// Delivered image size in pixels (after any downscale).
    pub width: u32,
    pub height: u32,
    /// Screen coordinate of the image's top-left corner.
    pub origin: (f64, f64),
    /// Size, in screen coordinates, of the area the image covers.
    pub screen_size: (f64, f64),
    /// Pixel size before downscaling (equals width/height when untouched).
    pub original: (u32, u32),
    /// Raw pixels of a tiny thumbnail, used to decide whether the frame is
    /// *visually* the same as the last one. Empty when the backend cannot
    /// produce one (dedup is then skipped rather than guessed).
    ///
    /// Exact byte comparison of full frames is useless on a live desktop — the
    /// menu-bar clock, the cursor and blinking carets change every frame. At
    /// thumbnail scale that noise averages out: measured mean-absolute
    /// difference between two idle captures is ~0.002/255.
    pub signature: Vec<u8>,
}

impl CaptureResult {
    /// Multiply an image coordinate by this and add [`Self::origin`] to get a
    /// screen coordinate. Returns `1.0` if the image has no width.
    pub fn scale_x(&self) -> f64 {
        if self.width == 0 {
            1.0
        } else {
            self.screen_size.0 / self.width as f64
        }
    }
    pub fn scale_y(&self) -> f64 {
        if self.height == 0 {
            1.0
        } else {
            self.screen_size.1 / self.height as f64
        }
    }
}

/// Per-capture options.
#[derive(Debug, Clone, Copy, Default)]
pub struct CaptureOpts {
    /// Longest delivered edge in pixels. `None` = backend default.
    pub max_edge: Option<u32>,
}

/// How much detail a capture needs.
///
/// The pixel budget for each tier lives in `VisionConfig`, not here: it is an
/// operator-tunable cost/legibility trade-off, and baking it into the enum
/// would put it out of reach of `config.toml`.
///
/// Image cost is driven by *pixel dimensions* (roughly `w * h / 750` tokens),
/// not by file size — so choosing a smaller edge is the only thing that
/// actually reduces spend. Most checks ("did the dialog appear?") do not need
/// text-legible resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detail {
    /// ~768px — layout/state checks. Cheapest.
    Low,
    /// ~1024px — most interactions.
    Balanced,
    /// ~1568px — small text stays legible. Default.
    Full,
}

impl Detail {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "low" => Self::Low,
            "balanced" | "medium" => Self::Balanced,
            "full" | "high" => Self::Full,
            _ => return None,
        })
    }
}

/// Why a capture/display query failed.
#[derive(Debug, Clone)]
pub enum VisionError {
    PermissionDenied(String),
    NotFound(String),
    Unsupported(String),
    Failed(String),
}

/// Platform screen-capture / display backend.
#[async_trait]
pub trait VisionBackend: Send + Sync {
    async fn list_displays(&self) -> Result<Vec<DisplayInfo>, VisionError>;
    async fn capture_screen(
        &self,
        display: Option<u32>,
        region: Option<(f64, f64, f64, f64)>,
        opts: CaptureOpts,
    ) -> Result<CaptureResult, VisionError>;
    async fn capture_window(
        &self,
        window_id: u32,
        opts: CaptureOpts,
    ) -> Result<CaptureResult, VisionError>;
    fn platform(&self) -> &'static str;

    /// Read text off the screen.
    ///
    /// The fallback for surfaces that expose no accessibility tree — canvases,
    /// games, custom-drawn and some Electron UI — where today the only advice
    /// is "take a screenshot and look at it", which costs vision tokens on
    /// every turn and gives back no coordinates to act on.
    ///
    /// Default: not available. A backend that cannot do this must say so
    /// rather than have every caller check first.
    async fn ocr(&self, _target: OcrTarget, _opts: &OcrOpts) -> Result<OcrResult, VisionError> {
        Err(VisionError::Unsupported(
            "text recognition is not available on this platform".into(),
        ))
    }
}

/// What to read text from.
#[derive(Debug, Clone, Copy)]
pub enum OcrTarget {
    /// A whole display (`None` = the main one).
    Display(Option<u32>),
    /// A screen rectangle, in points.
    Region((f64, f64, f64, f64)),
    /// One window, by the id `list_windows` reports.
    Window(u32),
}

/// Recognition options.
#[derive(Debug, Clone, Default)]
pub struct OcrOpts {
    /// BCP-47 languages to recognise. Empty = the system default.
    pub languages: Vec<String>,
    /// Trade accuracy for speed, and skip language correction with it.
    pub fast: bool,
    /// Drop lines the recogniser is less sure about than this (0.0–1.0).
    pub min_confidence: f64,
}

/// One recognised line, in **image pixels** with a top-left origin.
#[derive(Debug, Clone)]
pub struct OcrLine {
    pub text: String,
    pub confidence: f64,
    /// `(x, y, w, h)` in image pixels.
    pub px: (f64, f64, f64, f64),
}

/// Recognised text plus everything needed to map it back to the screen.
#[derive(Debug, Clone)]
pub struct OcrResult {
    pub lines: Vec<OcrLine>,
    pub width: u32,
    pub height: u32,
    /// Screen coordinate of the image's top-left corner.
    pub origin: (f64, f64),
    /// Size, in screen coordinates, of the area the image covers.
    pub screen_size: (f64, f64),
}

impl OcrResult {
    /// Same mapping as [`CaptureResult`]: image pixels are not screen points,
    /// and on a Retina display they differ by a factor of two.
    pub fn scale_x(&self) -> f64 {
        if self.width == 0 {
            1.0
        } else {
            self.screen_size.0 / self.width as f64
        }
    }
    pub fn scale_y(&self) -> f64 {
        if self.height == 0 {
            1.0
        } else {
            self.screen_size.1 / self.height as f64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(w: u32, h: u32, origin: (f64, f64), screen: (f64, f64)) -> CaptureResult {
        CaptureResult {
            mime_type: "image/png".into(),
            base64: String::new(),
            width: w,
            height: h,
            origin,
            screen_size: screen,
            original: (w, h),
            signature: Vec::new(),
        }
    }

    /// A Retina region capture is 2x the coordinate space: an image pixel is
    /// half a screen point. Passing image pixels straight to `mouse_action`
    /// would click at double the intended offset.
    #[test]
    fn retina_capture_maps_pixels_back_to_half_scale_points() {
        let c = cap(800, 600, (0.0, 0.0), (400.0, 300.0));
        assert_eq!(c.scale_x(), 0.5);
        assert_eq!(c.scale_y(), 0.5);
        let screen_x = c.origin.0 + 799.0 * c.scale_x();
        assert!((screen_x - 399.5).abs() < 1e-9);
    }

    /// A region away from the origin must offset, not just scale.
    #[test]
    fn region_origin_is_added_to_the_mapped_point() {
        let c = cap(200, 100, (500.0, 250.0), (100.0, 50.0));
        assert_eq!(c.origin.0 + 100.0 * c.scale_x(), 550.0);
        assert_eq!(c.origin.1 + 50.0 * c.scale_y(), 275.0);
    }

    /// Downscaling must not change where a point lands.
    #[test]
    fn downscaled_image_still_maps_to_the_same_screen_point() {
        let full = cap(3024, 1964, (0.0, 0.0), (1512.0, 982.0));
        let small = cap(1568, 1018, (0.0, 0.0), (1512.0, 982.0));
        // The same relative position resolves to the same screen point.
        let a = full.origin.0 + (full.width as f64 * 0.25) * full.scale_x();
        let b = small.origin.0 + (small.width as f64 * 0.25) * small.scale_x();
        assert!((a - b).abs() < 1e-9, "{a} vs {b}");
        assert!((a - 378.0).abs() < 1e-9);
    }

    #[test]
    fn zero_sized_image_does_not_divide_by_zero() {
        let c = cap(0, 0, (0.0, 0.0), (100.0, 100.0));
        assert_eq!(c.scale_x(), 1.0);
        assert_eq!(c.scale_y(), 1.0);
    }
}
