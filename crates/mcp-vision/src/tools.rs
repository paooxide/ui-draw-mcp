use std::sync::Arc;

use async_trait::async_trait;
use mcp_types::{
    CallCtx, Category, Envelope, ErrorCode, ImageContent, Tier, ToolDescriptor, ToolModule,
};
use serde_json::{json, Value};

use std::sync::Mutex;

use crate::backend::{
    CaptureOpts, CaptureResult, Detail, OcrLine, OcrOpts, OcrTarget, VisionBackend, VisionError,
};
use crate::config::VisionConfig;

/// The `vision` capture engine: `list_displays`, `capture_screen`,
/// `capture_window`. Images are returned as MCP image content blocks.
/// Cost bookkeeping across a session, and the last frame seen per capture
/// source so identical frames can be skipped.
#[derive(Default)]
struct CaptureState {
    counter: u64,
    /// `source key -> (thumbnail signature, capture id)`.
    last: std::collections::HashMap<String, (Vec<u8>, String)>,
    /// Cumulative image tokens actually spent this session.
    tokens: u64,
    /// Image tokens avoided by returning "unchanged" instead of a frame.
    tokens_saved: u64,
}

pub struct VisionModule {
    backend: Arc<dyn VisionBackend>,
    cfg: VisionConfig,
    state: Mutex<CaptureState>,
}

/// Mean absolute difference (0-255) between two thumbnail signatures.
/// `None` when they cannot be compared (missing or mismatched shape).
fn signature_distance(a: &[u8], b: &[u8]) -> Option<f64> {
    if a.is_empty() || a.len() != b.len() {
        return None;
    }
    let sum: u64 = a.iter().zip(b).map(|(x, y)| x.abs_diff(*y) as u64).sum();
    Some(sum as f64 / a.len() as f64)
}

impl VisionModule {
    pub fn new(backend: Arc<dyn VisionBackend>, cfg: VisionConfig) -> Self {
        VisionModule {
            backend,
            cfg,
            state: Mutex::new(CaptureState::default()),
        }
    }

    /// Read `detail` / `max_edge` into capture options.
    // Envelope is intentionally large (it can carry an image); it is the Err
    // type here only as a control-flow shortcut for a bad argument.
    #[allow(clippy::result_large_err)]
    fn opts_from(&self, args: &Value) -> Result<CaptureOpts, Envelope> {
        if let Some(px) = args.get("max_edge").and_then(Value::as_u64) {
            return Ok(CaptureOpts {
                max_edge: Some(px as u32),
            });
        }
        // With no argument the configured default tier is sent explicitly,
        // rather than leaving the backend to pick: so `config.toml` is the one
        // place that decides, on every platform.
        let detail = match args.get("detail").and_then(Value::as_str) {
            None => self.cfg.default_detail,
            Some(d) => Detail::parse(d).ok_or_else(|| {
                Envelope::fail(
                    "capture_screen",
                    ErrorCode::InvalidArgs,
                    "detail must be one of: low, balanced, full",
                )
            })?,
        };
        Ok(CaptureOpts {
            max_edge: Some(self.cfg.max_edge(detail)),
        })
    }

    /// Build the result, skipping the image entirely when this exact frame was
    /// already delivered for the same source.
    ///
    /// Polling loops ("has the dialog appeared yet?") otherwise re-send an
    /// identical megabyte every iteration. The agent already has the previous
    /// frame in context, so returning `unchanged` with the earlier capture id
    /// costs **zero image tokens** and loses nothing. `force: true` overrides.
    fn image_envelope(
        &self,
        tool: &str,
        cap: CaptureResult,
        mut data: Value,
        source: &str,
        force: bool,
    ) -> Envelope {
        let tokens = self.cfg.image_tokens(cap.width, cap.height);
        {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if !force {
                if let Some((prev_sig, prev_id)) = st.last.get(source) {
                    let dist = signature_distance(&cap.signature, prev_sig);
                    if dist.is_some_and(|d| d < self.cfg.unchanged_mad) {
                        let prev_id = prev_id.clone();
                        st.tokens_saved += tokens;
                        let saved = st.tokens_saved;
                        let spent = st.tokens;
                        drop(st);
                        data["unchanged"] = json!(true);
                        data["same_as"] = json!(prev_id);
                        data["width"] = json!(cap.width);
                        data["height"] = json!(cap.height);
                        data["note"] = json!(
                            "pixel-identical to the previous capture of this source; image omitted                              to save tokens: reuse the earlier one. Pass force=true to re-send."
                        );
                        data["session_image_tokens"] = json!(spent);
                        data["session_image_tokens_saved"] = json!(saved);
                        return Envelope::ok(tool, data);
                    }
                }
            }
            st.counter += 1;
            let id = format!("cap-{}", st.counter);
            st.tokens += tokens;
            st.last
                .insert(source.to_string(), (cap.signature.clone(), id.clone()));
            let (spent, saved) = (st.tokens, st.tokens_saved);
            drop(st);
            data["capture_id"] = json!(id);
            data["image_tokens"] = json!(tokens);
            data["session_image_tokens"] = json!(spent);
            data["session_image_tokens_saved"] = json!(saved);
        }
        let approx_bytes = cap.base64.len() / 4 * 3;
        data["width"] = json!(cap.width);
        data["height"] = json!(cap.height);
        data["bytes"] = json!(approx_bytes);
        // Image pixels are not screen coordinates: the capture is Retina-scaled
        // and may have been downscaled. Publish the mapping so a point read off
        // the image can be converted for mouse_action/scroll instead of being
        // passed through and landing in the wrong place.
        data["coordinate_mapping"] = json!({
            "origin_x": cap.origin.0,
            "origin_y": cap.origin.1,
            "scale_x": cap.scale_x(),
            "scale_y": cap.scale_y(),
            "note": "screen_x = origin_x + image_x * scale_x; screen_y = origin_y + image_y * scale_y",
        });
        if cap.original != (cap.width, cap.height) {
            data["downscaled_from"] = json!({
                "width": cap.original.0,
                "height": cap.original.1,
            });
        }
        if approx_bytes > self.cfg.max_image_bytes {
            data["warning"] = json!(format!(
                "image is {approx_bytes} bytes (> max_image_bytes {}); capture a region to shrink it",
                self.cfg.max_image_bytes
            ));
        }
        Envelope::ok_image(
            tool,
            data,
            ImageContent {
                mime_type: cap.mime_type,
                base64: cap.base64,
            },
        )
    }

    async fn capture_screen(&self, args: &Value) -> Envelope {
        let display = args
            .get("display")
            .and_then(Value::as_u64)
            .map(|n| n as u32);
        let region = args.get("region").and_then(parse_region);
        let opts = match self.opts_from(args) {
            Ok(o) => o,
            Err(e) => return e,
        };
        let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
        let source = match region {
            Some((x, y, w, h)) => format!("region:{x},{y},{w},{h}"),
            None => format!("screen:{}", display.map(|d| d as i64).unwrap_or(-1)),
        };
        match self.backend.capture_screen(display, region, opts).await {
            Ok(cap) => self.image_envelope(
                "capture_screen",
                cap,
                json!({ "display": display }),
                &source,
                force,
            ),
            Err(e) => vision_err("capture_screen", e),
        }
    }

    async fn capture_window(&self, args: &Value) -> Envelope {
        let Some(window_id) = args.get("window_id").and_then(Value::as_u64) else {
            return Envelope::fail(
                "capture_window",
                ErrorCode::InvalidArgs,
                "missing 'window_id'",
            );
        };
        let opts = match self.opts_from(args) {
            Ok(o) => o,
            Err(e) => return e,
        };
        let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
        match self.backend.capture_window(window_id as u32, opts).await {
            Ok(cap) => self.image_envelope(
                "capture_window",
                cap,
                json!({ "window_id": window_id }),
                &format!("window:{window_id}"),
                force,
            ),
            Err(e) => vision_err("capture_window", e),
        }
    }
}

impl VisionModule {
    /// Read text off the screen.
    async fn ocr_region(&self, args: &Value) -> Envelope {
        let tool = "ocr_region";
        let region = args.get("region").and_then(|r| {
            let f = |k: &str| r.get(k).and_then(Value::as_f64);
            match (f("x"), f("y"), f("w"), f("h")) {
                (Some(x), Some(y), Some(w), Some(h)) => Some((x, y, w, h)),
                _ => None,
            }
        });
        if args.get("region").is_some() && region.is_none() {
            return Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                "'region' needs numeric x, y, w and h",
            );
        }
        if let Some((_, _, w, h)) = region {
            if w <= 0.0 || h <= 0.0 {
                return Envelope::fail(
                    tool,
                    ErrorCode::InvalidArgs,
                    "'region' must have positive width and height",
                );
            }
        }
        let window_id = args.get("window_id").and_then(Value::as_u64);
        let target = match (region, window_id) {
            (Some(r), _) => OcrTarget::Region(r),
            (None, Some(id)) => OcrTarget::Window(id as u32),
            (None, None) => OcrTarget::Display(
                args.get("display")
                    .and_then(Value::as_u64)
                    .map(|d| d as u32),
            ),
        };

        let level = args
            .get("level")
            .and_then(Value::as_str)
            .unwrap_or("accurate");
        let fast = match level {
            "accurate" => false,
            "fast" => true,
            other => {
                return Envelope::fail(
                    tool,
                    ErrorCode::InvalidArgs,
                    format!("unknown level '{other}' (accurate|fast)"),
                )
            }
        };
        let opts = OcrOpts {
            languages: args
                .get("lang")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            fast,
            min_confidence: args
                .get("min_confidence")
                .and_then(Value::as_f64)
                .unwrap_or(0.3)
                .clamp(0.0, 1.0),
        };

        let mut res = match self.backend.ocr(target, &opts).await {
            Ok(r) => r,
            Err(e) => return vision_err(tool, e),
        };
        order_lines(&mut res.lines);
        let (sx, sy) = (res.scale_x(), res.scale_y());
        let lines: Vec<Value> = res
            .lines
            .iter()
            .map(|l| {
                let (x, y, w, h) = px_to_screen(l.px, res.origin, sx, sy);
                json!({
                    "text": l.text,
                    "confidence": (l.confidence * 1000.0).round() / 1000.0,
                    "bounds": { "x": x, "y": y, "w": w, "h": h },
                    // Pre-computed because it is what a click needs, and
                    // computing it from bounds is a step an agent can get wrong.
                    "center": { "x": x + w / 2.0, "y": y + h / 2.0 },
                })
            })
            .collect();
        let text = res
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        Envelope::ok(
            tool,
            json!({
                "text": text,
                "lines": lines,
                "line_count": lines.len(),
                // Said explicitly, because the obvious assumption: that these
                // are image pixels: would put every click in the wrong place.
                "coordinate_space": "screen",
                "region": { "x": res.origin.0, "y": res.origin.1,
                            "w": res.screen_size.0, "h": res.screen_size.1 },
                "image": { "width": res.width, "height": res.height },
                "engine": "apple-vision",
            }),
        )
    }
}

/// Group recognised lines into reading order: rows by vertical overlap, then
/// left to right within a row.
///
/// Vision returns observations in its own order, which is not the order a
/// person reads them in. An agent handed a jumbled transcript has to reason
/// about geometry it cannot see.
fn order_lines(lines: &mut [OcrLine]) {
    lines.sort_by(|a, b| {
        let row_height = a.px.3.max(b.px.3).max(1.0);
        let dy = (a.px.1 - b.px.1).abs();
        if dy < row_height * 0.5 {
            a.px.0
                .partial_cmp(&b.px.0)
                .unwrap_or(std::cmp::Ordering::Equal)
        } else {
            a.px.1
                .partial_cmp(&b.px.1)
                .unwrap_or(std::cmp::Ordering::Equal)
        }
    });
}

/// Map an image-pixel box to screen points.
///
/// Image pixels are not screen points: a Retina capture is already twice the
/// logical space. Returning pixel boxes would give an agent coordinates that
/// land in the wrong place when passed to `mouse_action`, which is the whole
/// reason to return boxes at all.
fn px_to_screen(
    px: (f64, f64, f64, f64),
    origin: (f64, f64),
    sx: f64,
    sy: f64,
) -> (f64, f64, f64, f64) {
    (
        origin.0 + px.0 * sx,
        origin.1 + px.1 * sy,
        px.2 * sx,
        px.3 * sy,
    )
}

#[async_trait]
impl ToolModule for VisionModule {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        vec![
            ToolDescriptor::new(
                "list_displays",
                Category::Vision,
                Tier::Read,
                "List displays (monitors) with geometry, scale, and which is primary.",
                json!({ "type": "object", "properties": {}, "required": [] }),
            ),
            ToolDescriptor::new(
                "capture_screen",
                Category::Vision,
                Tier::Read,
                "Capture a display (or a region) as a PNG image. Large captures are downscaled \
                 for token cost; the result carries a coordinate_mapping for converting a \
                 point on the image into a screen point for mouse_action/scroll.",
                json!({
                    "type": "object",
                    "properties": {
                        "display": { "type": "integer", "description": "display index from list_displays" },
                        "region": {
                            "type": "object",
                            "description": "capture just this screen rect: far cheaper than a full screen",
                            "properties": {
                                "x": {"type":"number"}, "y": {"type":"number"},
                                "w": {"type":"number"}, "h": {"type":"number"}
                            }
                        },
                        "detail": {
                            "type": "string",
                            "enum": ["low", "balanced", "full"],
                            "description": "low ~768px (state checks, cheapest), balanced ~1024px, full ~1568px (default, small text legible). Cost scales with pixel area."
                        },
                        "max_edge": { "type": "integer", "description": "explicit longest-edge override in px" },
                        "force": { "type": "boolean", "description": "re-send even if pixel-identical to the last capture of this source" }
                    },
                    "required": []
                }),
            ).untrusted_output(),
            ToolDescriptor::new(
                "ocr_region",
                Category::Vision,
                Tier::Read,
                "Read text off the screen, with a box for each line in screen coordinates. \
                 The fallback when get_ui_tree comes back sparse: a canvas, a game, a \
                 custom-drawn or Electron UI: where the text is visible but not in the \
                 accessibility tree. Cheaper than a screenshot for reading, and unlike a \
                 screenshot it hands back coordinates you can click.",
                json!({
                    "type": "object",
                    "properties": {
                        "region": {
                            "type": "object",
                            "description": "screen rectangle in points; omit for a whole display",
                            "properties": {
                                "x": { "type": "number" }, "y": { "type": "number" },
                                "w": { "type": "number" }, "h": { "type": "number" }
                            }
                        },
                        "window_id": { "type": "integer", "description": "read one window (id from list_windows)" },
                        "display": { "type": "integer", "description": "which display, when no region or window is given" },
                        "lang": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "BCP-47 languages, e.g. [\"en-US\"]; default is the system's"
                        },
                        "level": {
                            "type": "string",
                            "enum": ["accurate", "fast"],
                            "description": "accuracy versus speed (default accurate)"
                        },
                        "min_confidence": {
                            "type": "number",
                            "description": "drop lines below this confidence, 0-1 (default 0.3)"
                        }
                    },
                    "required": []
                }),
            ).untrusted_output(),
            ToolDescriptor::new(
                "capture_window",
                Category::Vision,
                Tier::Read,
                "Capture a single window as a PNG image: prefer this over capture_screen when \
                 you only care about one app; fewer pixels means proportionally fewer tokens.",
                json!({
                    "type": "object",
                    "properties": {
                        "window_id": { "type": "integer" },
                        "detail": { "type": "string", "enum": ["low", "balanced", "full"] },
                        "max_edge": { "type": "integer" },
                        "force": { "type": "boolean" }
                    },
                    "required": ["window_id"]
                }),
            ).untrusted_output(),
        ]
    }

    async fn call(&self, name: &str, args: Value, _ctx: &CallCtx) -> Envelope {
        match name {
            "list_displays" => match self.backend.list_displays().await {
                Ok(ds) => Envelope::ok("list_displays", json!({ "displays": ds })),
                Err(e) => vision_err("list_displays", e),
            },
            "capture_screen" => self.capture_screen(&args).await,
            "capture_window" => self.capture_window(&args).await,
            "ocr_region" => self.ocr_region(&args).await,
            other => Envelope::fail(other, ErrorCode::InvalidArgs, "unknown tool"),
        }
    }
}

fn parse_region(v: &Value) -> Option<(f64, f64, f64, f64)> {
    Some((
        v.get("x")?.as_f64()?,
        v.get("y")?.as_f64()?,
        v.get("w")?.as_f64()?,
        v.get("h")?.as_f64()?,
    ))
}

fn vision_err(tool: &str, e: VisionError) -> Envelope {
    let (code, msg) = match e {
        VisionError::PermissionDenied(m) => (ErrorCode::PermDenied, m),
        VisionError::NotFound(m) => (ErrorCode::NotFound, m),
        VisionError::Unsupported(m) => (ErrorCode::UnsupportedOs, m),
        VisionError::Failed(m) => (ErrorCode::ActionFailed, m),
    };
    Envelope::fail(tool, code, msg)
}

#[cfg(test)]
mod ocr_tests {
    use super::*;

    fn line(text: &str, px: (f64, f64, f64, f64)) -> OcrLine {
        OcrLine {
            text: text.into(),
            confidence: 1.0,
            px,
        }
    }

    /// The mapping that makes OCR boxes clickable. On a Retina display the
    /// capture is twice the logical space, so returning pixel coordinates
    /// would put every click at double the intended offset.
    #[test]
    fn boxes_map_from_image_pixels_to_screen_points() {
        // A 400x300 region captured at 800x600: scale 0.5 both ways.
        let (x, y, w, h) = px_to_screen((100.0, 40.0, 200.0, 20.0), (250.0, 102.0), 0.5, 0.5);
        assert_eq!((x, y, w, h), (300.0, 122.0, 100.0, 10.0));
    }

    #[test]
    fn a_non_retina_capture_maps_one_to_one() {
        let out = px_to_screen((10.0, 20.0, 30.0, 40.0), (0.0, 0.0), 1.0, 1.0);
        assert_eq!(out, (10.0, 20.0, 30.0, 40.0));
    }

    /// Vision returns observations in its own order. An agent handed a jumbled
    /// transcript has to reason about geometry it cannot see.
    #[test]
    fn lines_come_back_in_reading_order() {
        let mut lines = vec![
            line("world", (200.0, 10.0, 50.0, 12.0)),
            line("second row", (10.0, 40.0, 90.0, 12.0)),
            line("hello", (10.0, 10.0, 50.0, 12.0)),
        ];
        order_lines(&mut lines);
        let got: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(got, vec!["hello", "world", "second row"]);
    }

    /// Text on the same visual row is one row even when the baselines differ
    /// slightly, which they always do.
    #[test]
    fn a_small_vertical_difference_is_still_the_same_row() {
        let mut lines = vec![
            line("right", (200.0, 12.0, 40.0, 12.0)),
            line("left", (10.0, 10.0, 40.0, 12.0)),
        ];
        order_lines(&mut lines);
        assert_eq!(lines[0].text, "left");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detail_levels_parse_and_reject_junk() {
        let c = VisionConfig::default();
        assert_eq!(c.max_edge(Detail::parse("low").unwrap()), 768);
        assert_eq!(c.max_edge(Detail::parse("balanced").unwrap()), 1024);
        assert_eq!(c.max_edge(Detail::parse("medium").unwrap()), 1024);
        assert_eq!(c.max_edge(Detail::parse("full").unwrap()), 1568);
        assert_eq!(c.max_edge(Detail::parse("high").unwrap()), 1568);
        assert!(Detail::parse("enormous").is_none());
    }

    #[test]
    fn opts_prefer_explicit_max_edge_over_detail() {
        let m = VisionModule::new(Arc::new(StubBackend::default()), VisionConfig::default());
        let o = m
            .opts_from(&json!({"detail": "low", "max_edge": 900}))
            .unwrap();
        assert_eq!(o.max_edge, Some(900));
        let o = m.opts_from(&json!({"detail": "low"})).unwrap();
        assert_eq!(o.max_edge, Some(768));
        assert!(m.opts_from(&json!({"detail": "nope"})).is_err());
    }

    /// With no argument the *configured* default tier is what gets sent, so an
    /// operator can lower the baseline cost of every capture without touching
    /// a single call site.
    #[test]
    fn the_default_tier_comes_from_config() {
        let m = VisionModule::new(Arc::new(StubBackend::default()), VisionConfig::default());
        assert_eq!(m.opts_from(&json!({})).unwrap().max_edge, Some(1568));

        let thrifty = VisionConfig {
            default_detail: Detail::Low,
            detail_low_px: 640,
            ..VisionConfig::default()
        };
        let m = VisionModule::new(Arc::new(StubBackend::default()), thrifty);
        assert_eq!(m.opts_from(&json!({})).unwrap().max_edge, Some(640));
        assert_eq!(
            m.opts_from(&json!({"detail": "full"})).unwrap().max_edge,
            Some(1568)
        );
    }

    /// The comparison must tolerate the small per-frame noise a live desktop
    /// always produces (clock, cursor, caret) while still catching real change.
    #[test]
    fn signature_distance_tolerates_noise_but_catches_change() {
        let base = vec![100u8; 2560];
        assert_eq!(signature_distance(&base, &base), Some(0.0));

        // Idle jitter: a handful of pixels move slightly.
        let mut jittered = base.clone();
        for i in (0..jittered.len()).step_by(500) {
            jittered[i] = 104;
        }
        let d = signature_distance(&base, &jittered).unwrap();
        assert!(
            d < VisionConfig::default().unchanged_mad,
            "idle jitter should read as unchanged: {d}"
        );

        // A dialog opening changes a large area substantially.
        let mut changed = base.clone();
        for b in changed.iter_mut().take(600) {
            *b = 250;
        }
        let d = signature_distance(&base, &changed).unwrap();
        assert!(
            d > VisionConfig::default().unchanged_mad,
            "a real change must be detected: {d}"
        );
    }

    /// Without a signature we must not guess: dedup is skipped.
    #[test]
    fn missing_or_mismatched_signature_is_incomparable() {
        assert_eq!(signature_distance(&[], &[]), None);
        assert_eq!(signature_distance(&[1, 2, 3], &[1, 2]), None);
    }

    #[derive(Default)]
    struct StubBackend {
        payload: std::sync::Mutex<String>,
    }

    #[async_trait]
    impl VisionBackend for StubBackend {
        async fn list_displays(&self) -> Result<Vec<crate::DisplayInfo>, VisionError> {
            Ok(vec![])
        }
        async fn capture_screen(
            &self,
            _d: Option<u32>,
            _r: Option<(f64, f64, f64, f64)>,
            _o: CaptureOpts,
        ) -> Result<CaptureResult, VisionError> {
            let payload = self.payload.lock().unwrap().clone();
            // Signature mirrors the payload so "screen changed" is expressible.
            let signature = vec![payload.as_bytes()[0]; 256];
            Ok(CaptureResult {
                mime_type: "image/png".into(),
                base64: payload,
                width: 1000,
                height: 750,
                origin: (0.0, 0.0),
                screen_size: (1000.0, 750.0),
                original: (1000, 750),
                signature,
            })
        }
        async fn capture_window(
            &self,
            _w: u32,
            _o: CaptureOpts,
        ) -> Result<CaptureResult, VisionError> {
            Err(VisionError::NotFound("n/a".into()))
        }
        fn platform(&self) -> &'static str {
            "stub"
        }
    }

    fn ctx() -> CallCtx {
        CallCtx::new("t", mcp_types::CancelToken::new())
    }

    /// The polling case: an unchanged screen must not re-bill a full frame.
    #[tokio::test]
    async fn repeat_capture_of_an_unchanged_screen_is_free() {
        let stub = Arc::new(StubBackend {
            payload: std::sync::Mutex::new("AAAA".repeat(64)),
        });
        let m = VisionModule::new(stub.clone(), VisionConfig::default());

        let first = m.call("capture_screen", json!({}), &ctx()).await;
        let d1 = first.data.unwrap();
        assert!(
            d1.get("unchanged").is_none(),
            "first capture must send bytes"
        );
        let billed = d1["image_tokens"].as_u64().unwrap();
        assert_eq!(billed, VisionConfig::default().image_tokens(1000, 750));

        let second = m.call("capture_screen", json!({}), &ctx()).await;
        let d2 = second.data.unwrap();
        assert_eq!(d2["unchanged"], json!(true));
        assert_eq!(d2["same_as"], d1["capture_id"]);
        // Nothing extra was spent, and the avoided cost is reported.
        assert_eq!(d2["session_image_tokens"].as_u64().unwrap(), billed);
        assert_eq!(d2["session_image_tokens_saved"].as_u64().unwrap(), billed);

        // A changed screen must send again.
        *stub.payload.lock().unwrap() = "BBBB".repeat(64);
        let third = m.call("capture_screen", json!({}), &ctx()).await;
        let d3 = third.data.unwrap();
        assert!(
            d3.get("unchanged").is_none(),
            "changed screen must be re-sent"
        );
        assert_eq!(d3["session_image_tokens"].as_u64().unwrap(), billed * 2);
    }

    /// `force` must defeat dedup, for when the agent dropped the earlier frame.
    #[tokio::test]
    async fn force_resends_an_identical_frame() {
        let stub = Arc::new(StubBackend {
            payload: std::sync::Mutex::new("CCCC".repeat(64)),
        });
        let m = VisionModule::new(stub, VisionConfig::default());
        m.call("capture_screen", json!({}), &ctx()).await;
        let forced = m
            .call("capture_screen", json!({"force": true}), &ctx())
            .await;
        assert!(forced.data.unwrap().get("unchanged").is_none());
    }

    /// Different sources are tracked independently: a region capture must not
    /// be deduped against a full-screen one.
    #[tokio::test]
    async fn dedup_is_per_source() {
        let stub = Arc::new(StubBackend {
            payload: std::sync::Mutex::new("DDDD".repeat(64)),
        });
        let m = VisionModule::new(stub, VisionConfig::default());
        m.call("capture_screen", json!({}), &ctx()).await;
        let region = m
            .call(
                "capture_screen",
                json!({"region": {"x":0,"y":0,"w":10,"h":10}}),
                &ctx(),
            )
            .await;
        assert!(region.data.unwrap().get("unchanged").is_none());
    }
}
