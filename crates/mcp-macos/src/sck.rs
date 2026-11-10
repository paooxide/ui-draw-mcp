//! Window capture through ScreenCaptureKit.
//!
//! `screencapture -R` photographs a rectangle of the screen, so a window that
//! is covered by another one comes back as the covering window. ScreenCaptureKit
//! renders one window from the window server's own copy of its contents, so
//! what is in front of it does not matter, and nothing is spawned or written to
//! disk. It needs macOS 14 (`SCScreenshotManager`) and the Screen Recording
//! permission; [`should_fall_back`] says which failures are worth retrying with
//! the old path.
//!
//! Every ScreenCaptureKit entry point is a completion handler that runs on a
//! queue of the framework's choosing. Nothing that is not `Send` leaves a
//! handler: the window is picked, rendered, encoded and measured inside it, and
//! only plain bytes and numbers cross back to the awaiting task.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyClass;
use objc2::AnyThread;
use objc2_core_foundation::{CFMutableData, CFString};
use objc2_core_graphics::{CGDataProvider, CGImage};
use objc2_foundation::NSError;
use objc2_image_io::CGImageDestination;
use objc2_screen_capture_kit::{
    SCContentFilter, SCScreenshotManager, SCShareableContent, SCStreamConfiguration, SCWindow,
};
use tokio::sync::oneshot;

/// Longest we wait for ScreenCaptureKit. The first call in a session can sit
/// behind a system permission prompt; a person who is not there to answer it
/// must get an error, not a hung tool call.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Samples per edge of the change-detection thumbnail.
const SIG_CELLS: usize = 32;

/// `SCStreamErrorUserDeclined`: the user has not granted Screen Recording.
const ERR_USER_DECLINED: i64 = -3801;
const SCK_ERROR_DOMAIN: &str = "com.apple.ScreenCaptureKit.SCStreamErrorDomain";

/// The window to capture: the window server's id for it, which is what the
/// caller resolved from the app and per-app window id it was given. It is
/// never looked up by frame or by "the frontmost app", so a covered or
/// background window cannot be mistaken for another.
#[derive(Debug, Clone)]
pub(crate) struct WindowTarget {
    /// The owning process; checked against the window, so an id that has
    /// since been reused by another application's window is not captured.
    pub pid: i32,
    /// `CGWindowID`.
    pub window_id: u32,
}

/// What ScreenCaptureKit knows about one window.
#[derive(Debug, Clone)]
pub(crate) struct Candidate {
    pub pid: i32,
    pub window_id: u32,
}

/// An image ready to hand to the agent.
#[derive(Debug)]
pub(crate) struct Captured {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Pixel size at the display's native scale, before any downscale.
    pub original: (u32, u32),
    pub signature: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SckFailure {
    /// Screen Recording is not granted. The old path cannot do better.
    PermissionDenied(String),
    /// No ScreenCaptureKit window matches the one asked for.
    NoSuchWindow,
    /// The API is missing (older macOS), timed out, or errored.
    Unavailable(String),
    /// ScreenCaptureKit answered, and the answer is not an image of the window.
    Failed(String),
}

/// Whether the `screencapture` path is worth trying after this failure.
///
/// A missing permission fails the same way there. A blank render is an answer
/// about the window (it has nothing drawn), and retrying would photograph
/// whatever is in front of it instead.
pub(crate) fn should_fall_back(f: &SckFailure) -> bool {
    matches!(f, SckFailure::NoSuchWindow | SckFailure::Unavailable(_))
}

/// Map a ScreenCaptureKit `NSError` onto a failure.
pub(crate) fn classify_error(domain: &str, code: i64, description: &str) -> SckFailure {
    if domain == SCK_ERROR_DOMAIN && code == ERR_USER_DECLINED {
        return SckFailure::PermissionDenied(
            "ScreenCaptureKit refused: grant Screen Recording permission to the app running \
             agentctl (System Settings > Privacy & Security > Screen Recording)"
                .into(),
        );
    }
    SckFailure::Unavailable(format!("ScreenCaptureKit error {code}: {description}"))
}

/// Choose the ScreenCaptureKit window that is the target: the one with its
/// id, provided the same process still owns it.
pub(crate) fn pick_window(cands: &[Candidate], target: &WindowTarget) -> Option<usize> {
    cands
        .iter()
        .position(|c| c.window_id == target.window_id && c.pid == target.pid)
}

/// Pixel size of a window of `points` at `scale` pixels per point.
pub(crate) fn native_pixels(points: (f64, f64), scale: f64) -> (u32, u32) {
    let px = |p: f64| (p * scale).round().max(1.0) as u32;
    (px(points.0), px(points.1))
}

/// Shrink `native` so its longest edge is at most `max_edge`, keeping the
/// aspect ratio and never enlarging. Same rule as `sips -Z`, which the
/// `screencapture` path uses, so both paths deliver the same dimensions.
pub(crate) fn fit_edge(native: (u32, u32), max_edge: u32) -> (u32, u32) {
    let longest = native.0.max(native.1);
    if longest <= max_edge {
        return native;
    }
    let f = max_edge as f64 / longest as f64;
    let d = |v: u32| ((v as f64 * f).round() as u32).max(1);
    (d(native.0), d(native.1))
}

/// True when no pixel has any alpha: the window server had nothing to draw
/// (a minimised or never-shown window). Reporting that as an image would be a
/// blank frame passed off as success.
pub(crate) fn all_transparent(px: &[u8], w: usize, h: usize, bytes_per_row: usize) -> bool {
    if w == 0 || h == 0 || bytes_per_row < w * 4 || px.len() < bytes_per_row * (h - 1) + w * 4 {
        return false;
    }
    (0..h).all(|y| {
        px[y * bytes_per_row..y * bytes_per_row + w * 4]
            .chunks_exact(4)
            .all(|p| p[3] == 0)
    })
}

/// A `SIG_CELLS` x `SIG_CELLS` box-averaged thumbnail of a 32-bit image, three
/// bytes per cell. Only ever compared with another signature from this
/// function, so channel order does not matter. Empty when the buffer does not
/// look like 32-bit pixels, which switches change detection off for the frame
/// rather than guessing.
pub(crate) fn thumbnail_signature(px: &[u8], w: usize, h: usize, bytes_per_row: usize) -> Vec<u8> {
    if w == 0 || h == 0 || bytes_per_row < w * 4 || px.len() < bytes_per_row * (h - 1) + w * 4 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(SIG_CELLS * SIG_CELLS * 3);
    for cy in 0..SIG_CELLS {
        let (y0, y1) = (
            cy * h / SIG_CELLS,
            ((cy + 1) * h / SIG_CELLS).max(cy * h / SIG_CELLS + 1),
        );
        for cx in 0..SIG_CELLS {
            let (x0, x1) = (
                cx * w / SIG_CELLS,
                ((cx + 1) * w / SIG_CELLS).max(cx * w / SIG_CELLS + 1),
            );
            let mut sum = [0u64; 3];
            let mut n = 0u64;
            for y in y0..y1.min(h) {
                for x in x0..x1.min(w) {
                    let o = y * bytes_per_row + x * 4;
                    for (s, v) in sum.iter_mut().zip(&px[o..o + 3]) {
                        *s += u64::from(*v);
                    }
                    n += 1;
                }
            }
            let n = n.max(1);
            out.extend(sum.iter().map(|s| (s / n) as u8));
        }
    }
    out
}

/// Sends the outcome exactly once, whichever handler gets there first.
struct Reply(Mutex<Option<oneshot::Sender<Result<Captured, SckFailure>>>>);

impl Reply {
    fn send(&self, r: Result<Captured, SckFailure>) {
        let tx = self.0.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(tx) = tx {
            // The receiver is gone after a timeout; nobody is left to tell.
            let _ = tx.send(r);
        }
    }
}

/// `SCScreenshotManager` arrived in macOS 14; on older systems the class does
/// not exist and messaging it would crash.
pub(crate) fn screenshot_api_available() -> bool {
    AnyClass::get(c"SCScreenshotManager").is_some()
}

/// Read an `NSError` the framework passed to a handler.
///
/// # Safety
/// `err` must be null or point to a live `NSError` for the duration of the call.
unsafe fn failure_from(err: *mut NSError) -> SckFailure {
    // SAFETY: caller guarantees validity; we only read it.
    match unsafe { err.as_ref() } {
        Some(e) => classify_error(
            &e.domain().to_string(),
            e.code() as i64,
            &e.localizedDescription().to_string(),
        ),
        None => {
            SckFailure::Failed("ScreenCaptureKit returned neither an image nor an error".into())
        }
    }
}

/// Encode `image` as PNG in memory.
fn encode_png(image: &CGImage) -> Option<Vec<u8>> {
    let data = CFMutableData::new(None, 0)?;
    let uti = CFString::from_static_str("public.png");
    // SAFETY: `data` outlives `dest`; no options dictionary is passed.
    let dest = unsafe { CGImageDestination::with_data(&data, &uti, 1, None) }?;
    // SAFETY: `image` is a valid CGImage borrowed for this call.
    unsafe { dest.add_image(image, None) };
    // SAFETY: the destination has its one image, and is finalised once.
    if !unsafe { dest.finalize() } {
        return None;
    }
    Some(data.to_vec())
}

/// Turn the image the framework delivered into the result.
///
/// # Safety
/// `image` and `err` must be null or valid for the duration of the call.
unsafe fn finish(
    image: *mut CGImage,
    err: *mut NSError,
    native: (u32, u32),
) -> Result<Captured, SckFailure> {
    // SAFETY: caller guarantees validity; the borrow ends with this call, so
    // the image is never retained past the handler.
    let Some(image) = (unsafe { image.as_ref() }) else {
        return Err(unsafe { failure_from(err) });
    };
    let (w, h) = (CGImage::width(Some(image)), CGImage::height(Some(image)));
    let signature = if CGImage::bits_per_pixel(Some(image)) == 32 {
        let bpr = CGImage::bytes_per_row(Some(image));
        let data = CGImage::data_provider(Some(image)).and_then(|p| CGDataProvider::data(Some(&p)));
        match data {
            Some(data) => {
                // SAFETY: `data` is owned here and not mutated while borrowed.
                let px = unsafe { data.as_bytes_unchecked() };
                if all_transparent(px, w, h, bpr) {
                    return Err(SckFailure::Failed(
                        "the window produced no pixels (minimised, or not drawn yet)".into(),
                    ));
                }
                thumbnail_signature(px, w, h, bpr)
            }
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };
    let png = encode_png(image)
        .ok_or_else(|| SckFailure::Failed("could not encode the capture as PNG".into()))?;
    Ok(Captured {
        png,
        width: w as u32,
        height: h as u32,
        original: native,
        signature,
    })
}

/// Runs inside the shareable-content handler: pick the window and ask for its
/// image; the second handler completes `reply`.
fn start_capture(
    content: &SCShareableContent,
    target: &WindowTarget,
    max_edge: u32,
    reply: &Arc<Reply>,
) {
    // SAFETY (all calls below): plain getters on live objects owned by this
    // handler invocation; nothing here is stored beyond it.
    let windows: Vec<Retained<SCWindow>> = unsafe { content.windows() }.to_vec();
    let cands: Vec<Candidate> = windows
        .iter()
        .map(|w| unsafe {
            Candidate {
                pid: w.owningApplication().map_or(-1, |a| a.processID()),
                window_id: w.windowID(),
            }
        })
        .collect();
    let Some(i) = pick_window(&cands, target) else {
        reply.send(Err(SckFailure::NoSuchWindow));
        return;
    };
    // SAFETY: `alloc` + `init` on a fresh object, window is live.
    let filter = unsafe {
        SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &windows[i])
    };
    // SAFETY: getters on the filter we just built.
    let (rect, scale) = unsafe { (filter.contentRect(), f64::from(filter.pointPixelScale())) };
    let native = native_pixels((rect.size.width, rect.size.height), scale);
    // Let the framework do the downscale: it renders straight to the size we
    // deliver, so no second image is made and no `sips` runs.
    let (w, h) = fit_edge(native, max_edge);
    // SAFETY: configuration setters on an object we own.
    let config = unsafe {
        let c = SCStreamConfiguration::new();
        c.setWidth(w as usize);
        c.setHeight(h as usize);
        c.setShowsCursor(false);
        // Frame-sized output, so pixels / points is the display scale and the
        // coordinate mapping other tools rely on holds.
        c.setIgnoreShadowsSingleWindow(true);
        c
    };
    let r = Arc::clone(reply);
    let done = RcBlock::new(move |image: *mut CGImage, err: *mut NSError| {
        // SAFETY: the framework passes a valid image or error for this call.
        r.send(unsafe { finish(image, err, native) });
    });
    // SAFETY: filter, config and block are live for the call; the framework
    // copies the block, so dropping ours afterwards is sound.
    unsafe {
        SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
            &filter,
            &config,
            Some(&done),
        );
    }
}

/// Capture one window, whatever is in front of it.
pub(crate) async fn capture_window(
    target: WindowTarget,
    max_edge: u32,
) -> Result<Captured, SckFailure> {
    if !screenshot_api_available() {
        return Err(SckFailure::Unavailable(
            "SCScreenshotManager needs macOS 14 or later".into(),
        ));
    }
    let (tx, rx) = oneshot::channel();
    let reply = Arc::new(Reply(Mutex::new(Some(tx))));
    {
        // Scoped so the (non-Send) block is gone before the first await.
        let r = Arc::clone(&reply);
        let handler = RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
            // SAFETY: the framework passes a valid object or error for this call.
            match unsafe { content.as_ref() } {
                Some(c) => start_capture(c, &target, max_edge, &r),
                None => r.send(Err(unsafe { failure_from(err) })),
            }
        });
        // Windows on other Spaces and minimised ones are included
        // (`onScreenWindowsOnly: false`); desktop elements are not.
        // SAFETY: the block is live for the call and copied by the framework.
        unsafe {
            SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
                true, false, &handler,
            );
        }
    }
    match tokio::time::timeout(TIMEOUT, rx).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => Err(SckFailure::Unavailable(
            "ScreenCaptureKit dropped the request".into(),
        )),
        Err(_) => Err(SckFailure::Unavailable(format!(
            "ScreenCaptureKit did not answer within {}s",
            TIMEOUT.as_secs()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(pid: i32, window_id: u32) -> Candidate {
        Candidate { pid, window_id }
    }

    fn target() -> WindowTarget {
        WindowTarget {
            pid: 10,
            window_id: 77,
        }
    }

    #[test]
    fn picks_the_window_with_the_resolved_id() {
        // Other windows of the same app, and the same id number owned by
        // nothing we asked about, are not it.
        let cands = [cand(10, 76), cand(11, 77), cand(10, 77), cand(10, 78)];
        assert_eq!(pick_window(&cands, &target()), Some(2));
    }

    #[test]
    fn an_id_now_owned_by_another_process_is_no_match() {
        assert_eq!(pick_window(&[cand(11, 77)], &target()), None);
        assert_eq!(pick_window(&[], &target()), None);
    }

    #[test]
    fn native_size_is_points_times_scale() {
        assert_eq!(native_pixels((800.0, 600.0), 2.0), (1600, 1200));
        assert_eq!(native_pixels((800.4, 600.0), 1.0), (800, 600));
        assert_eq!(native_pixels((0.0, 0.0), 2.0), (1, 1));
    }

    #[test]
    fn fit_edge_matches_sips_z_and_never_enlarges() {
        assert_eq!(fit_edge((3024, 1964), 1568), (1568, 1018));
        assert_eq!(fit_edge((1000, 4000), 1000), (250, 1000));
        assert_eq!(fit_edge((800, 600), 1568), (800, 600));
        assert_eq!(fit_edge((1568, 100), 1568), (1568, 100));
        assert_eq!(fit_edge((4096, 1), 160), (160, 1));
    }

    #[test]
    fn errors_map_to_permission_or_unavailable() {
        assert!(matches!(
            classify_error(SCK_ERROR_DOMAIN, -3801, "declined"),
            SckFailure::PermissionDenied(_)
        ));
        // The same code from another domain is not ours to interpret.
        assert!(matches!(
            classify_error("NSCocoaErrorDomain", -3801, "x"),
            SckFailure::Unavailable(_)
        ));
        assert!(matches!(
            classify_error(SCK_ERROR_DOMAIN, -3802, "failed to start"),
            SckFailure::Unavailable(_)
        ));
    }

    #[test]
    fn only_unavailability_and_unmatched_windows_fall_back() {
        assert!(should_fall_back(&SckFailure::NoSuchWindow));
        assert!(should_fall_back(&SckFailure::Unavailable(
            "old macOS".into()
        )));
        assert!(!should_fall_back(&SckFailure::PermissionDenied(
            "no".into()
        )));
        assert!(!should_fall_back(&SckFailure::Failed("blank".into())));
    }

    #[test]
    fn a_frame_with_no_alpha_is_blank() {
        let blank = vec![0u8; 4 * 4 * 3];
        assert!(all_transparent(&blank, 4, 3, 16));
        let mut one = blank.clone();
        one[4 * 4 + 4 + 3] = 255;
        assert!(!all_transparent(&one, 4, 3, 16));
        // Row padding is not pixels.
        let mut padded = vec![0u8; 20 * 2];
        padded[16..20].copy_from_slice(&[9, 9, 9, 9]);
        assert!(all_transparent(&padded, 4, 2, 20));
        // A buffer that cannot be judged is never called blank.
        assert!(!all_transparent(&[0u8; 3], 4, 3, 16));
    }

    #[test]
    fn signature_has_fixed_size_and_tracks_change() {
        let (w, h) = (64usize, 48usize);
        let flat = vec![100u8; w * h * 4];
        let a = thumbnail_signature(&flat, w, h, w * 4);
        assert_eq!(a.len(), SIG_CELLS * SIG_CELLS * 3);
        assert_eq!(a, thumbnail_signature(&flat, w, h, w * 4));
        let mut changed = flat.clone();
        for p in changed.chunks_exact_mut(4).take(w * h / 2) {
            p[..3].copy_from_slice(&[250, 250, 250]);
        }
        let b = thumbnail_signature(&changed, w, h, w * 4);
        assert_ne!(a, b);
        // Smaller than the grid still yields a full signature.
        assert_eq!(
            thumbnail_signature(&[1u8; 16], 2, 2, 8).len(),
            SIG_CELLS * SIG_CELLS * 3
        );
        assert!(thumbnail_signature(&[], 0, 0, 0).is_empty());
    }

    /// A real CGImage built by CoreGraphics (needs no display), pushed through
    /// the same conversion the ScreenCaptureKit handler uses: this is what
    /// proves the PNG is real and the pixel read honours the row stride.
    fn real_image(w: usize, h: usize, alpha: u8) -> CGImageBox {
        use objc2_core_graphics::{
            CGBitmapContextCreate, CGBitmapContextCreateImage, CGColorSpace, CGImageAlphaInfo,
            CGImageByteOrderInfo,
        };
        // 4 bytes per pixel, BGRA premultiplied: what ScreenCaptureKit delivers.
        let mut px = vec![0u8; w * 4 * h];
        for p in px.chunks_exact_mut(4) {
            p.copy_from_slice(&[10, 120, 200, alpha]);
        }
        let space = CGColorSpace::new_device_rgb().expect("colour space");
        let info = CGImageByteOrderInfo::Order32Little.0 | CGImageAlphaInfo::PremultipliedFirst.0;
        // SAFETY: `px` outlives the context and is `w * 4 * h` bytes, matching
        // the width, height and row bytes passed.
        let ctx = unsafe {
            CGBitmapContextCreate(px.as_mut_ptr().cast(), w, h, 8, w * 4, Some(&space), info)
        }
        .expect("bitmap context");
        CGImageBox(CGBitmapContextCreateImage(Some(&ctx)).expect("image"))
    }

    struct CGImageBox(objc2_core_foundation::CFRetained<CGImage>);

    #[test]
    fn a_real_cgimage_becomes_a_png_of_the_same_size() {
        let img = real_image(64, 48, 255);
        let ptr = &*img.0 as *const CGImage as *mut CGImage;
        // SAFETY: `img` is alive for the call and no error is passed.
        let c = unsafe { finish(ptr, std::ptr::null_mut(), (128, 96)) }.expect("converts");
        assert_eq!(&c.png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(u32::from_be_bytes(c.png[16..20].try_into().unwrap()), 64);
        assert_eq!(u32::from_be_bytes(c.png[20..24].try_into().unwrap()), 48);
        assert_eq!((c.width, c.height, c.original), (64, 48, (128, 96)));
        assert_eq!(c.signature.len(), SIG_CELLS * SIG_CELLS * 3);
    }

    #[test]
    fn a_real_blank_cgimage_is_refused() {
        let img = real_image(32, 32, 0);
        let ptr = &*img.0 as *const CGImage as *mut CGImage;
        // SAFETY: as above.
        let r = unsafe { finish(ptr, std::ptr::null_mut(), (32, 32)) };
        assert!(matches!(r, Err(SckFailure::Failed(_))), "{r:?}");
    }

    #[test]
    fn no_image_and_no_error_is_an_error() {
        // SAFETY: both pointers are null, which the function accepts.
        let r = unsafe { finish(std::ptr::null_mut(), std::ptr::null_mut(), (1, 1)) };
        assert!(matches!(r, Err(SckFailure::Failed(_))));
    }
}
