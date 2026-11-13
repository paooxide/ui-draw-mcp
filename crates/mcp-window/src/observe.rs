//! `observe`: what an agent needs to act on a window, in one call.
//!
//! An agent that meets an app it cannot read calls `get_ui_tree`, sees few
//! elements, calls `capture_window`, squints at the picture, calls `ocr_region`
//! and then works out which pixel to click. `observe` does the three reads
//! together and joins them: the accessibility tree comes back as `get_ui_tree`
//! returns it (same refs, usable by `ui_action`), and when the tree says almost
//! nothing about the window it also reads the window's text and returns each
//! piece either attached to the element it sits on or as a numbered mark with
//! the screen rectangle `mouse_action` takes. The join itself is
//! `mcp_vision::marks`; this module decides when it is needed (sparsity), turns
//! a snapshot into the join's inputs, and runs the reads.
//!
//! It is passive. The application is resolved without pinning it, the snapshot
//! is taken without pinning it, the window is captured through the same path
//! `capture_window` uses (ScreenCaptureKit where there is one, so a window
//! behind others is still read from its own pixels), and nothing is raised,
//! focused or clicked.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mcp_a11y::{
    flatten, A11yBackend, BackendError, FlattenConfig, Snapshot, SnapshotArena, SnapshotRequest,
    UiNode,
};
use mcp_types::{
    CallCtx, Category, Envelope, ErrorCode, ImageContent, Tier, ToolDescriptor, ToolModule,
};
use mcp_vision::marks::{
    draw_marks_b64, merge, screen_to_image, AxItem, AxKind, BoxSource, Candidate, MarkDraw,
    MergeOpts, Rect,
};
use mcp_vision::{
    CaptureOpts, Detail, OcrOpts, OcrTarget, VisionBackend, VisionConfig, VisionError,
};
use serde_json::{json, Value};

use crate::backend::{window_at, WindowBackend, WindowError, WindowState};

/// Fewer actionable elements than this in the window is a tree that says too
/// little to act on, however small the window.
pub const MIN_ACTIONABLE: usize = 5;

/// A window with more than one actionable element per this many square points
/// of its area is read as sparse: a 1400 x 900 window needs about eleven.
pub const AREA_PER_ACTIONABLE: f64 = 120_000.0;

/// A childless element covering at least this share of the window, of a kind
/// that holds content drawn rather than described (a canvas, a web view, an
/// empty group), blinds the tree to that share of the window.
pub const BLIND_SHARE: f64 = 0.25;

/// Roles that hold content the tree should describe but, when childless and
/// large, usually means the app drew it itself.
const BLIND_ROLES: &[&str] = &[
    "canvas",
    "webarea",
    "web",
    "group",
    "scrollarea",
    "image",
    "unknown",
    "opengl",
];

/// Most entries `max_elements` may ask for.
const MAX_ELEMENTS_CAP: usize = 300;
const DEFAULT_MAX_ELEMENTS: usize = 60;

/// One element reduced to what sparsity needs.
#[derive(Debug, Clone, PartialEq)]
pub struct SparseItem {
    /// Normalised role (`button`, `webarea`).
    pub role: String,
    pub rect: Rect,
    /// Whether it gets a ref: something an agent can act on.
    pub actionable: bool,
    pub children: usize,
}

/// Why a tree counts as sparse.
#[derive(Debug, Clone, PartialEq)]
pub enum SparseReason {
    /// Fewer than [`MIN_ACTIONABLE`] actionable elements.
    TooFew { actionable: usize },
    /// Actionable elements, but fewer than the window's area calls for.
    LowDensity { actionable: usize, needed: usize },
    /// A large childless element of this role hides the window's content.
    BlindRegion { role: String, share: f64 },
}

/// The verdict, with the figures behind it.
#[derive(Debug, Clone, PartialEq)]
pub struct Sparsity {
    pub reason: Option<SparseReason>,
    pub actionable: usize,
    pub window_area: f64,
}

impl Sparsity {
    pub fn sparse(&self) -> bool {
        self.reason.is_some()
    }

    fn describe(&self) -> Value {
        let reason = match &self.reason {
            None => json!(null),
            Some(SparseReason::TooFew { actionable }) => json!({
                "kind": "too_few_elements",
                "text": format!("{actionable} actionable elements, fewer than {MIN_ACTIONABLE}"),
            }),
            Some(SparseReason::LowDensity { actionable, needed }) => json!({
                "kind": "low_density",
                "text": format!(
                    "{actionable} actionable elements for this window's size; about {needed} are expected"
                ),
            }),
            Some(SparseReason::BlindRegion { role, share }) => json!({
                "kind": "blind_region",
                "text": format!(
                    "a childless '{role}' covers {:.0}% of the window, so the tree cannot describe it",
                    share * 100.0
                ),
            }),
        };
        json!({
            "sparse": self.sparse(),
            "reason": reason,
            "actionable": self.actionable,
            "window_area": self.window_area.round(),
            "rule": SPARSITY_RULE,
        })
    }
}

/// The rule, as the descriptor and every result state it.
pub const SPARSITY_RULE: &str =
    "sparse when the window has fewer than 5 actionable elements, or fewer than one per 120000 \
     square points of its area, or a childless canvas / web view / group / scroll area / image \
     covers at least 25% of it";

fn in_window(window: &Rect, r: &Rect) -> bool {
    window.contains_point(r.center())
}

/// Decide whether the accessibility tree is too thin to act on for `window`.
///
/// Only elements whose centre lies in the window count (a snapshot is of the
/// whole application, menu bar and other windows included). Pure.
pub fn assess_sparsity(window: &Rect, items: &[SparseItem]) -> Sparsity {
    let area = window.area();
    let inside: Vec<&SparseItem> = items
        .iter()
        .filter(|i| in_window(window, &i.rect))
        .collect();
    let actionable = inside.iter().filter(|i| i.actionable).count();
    let mut out = Sparsity {
        reason: None,
        actionable,
        window_area: area,
    };
    if area <= 0.0 {
        return out;
    }
    // The strongest evidence first: a region the tree cannot see into.
    let blind = inside
        .iter()
        .filter(|i| i.children == 0 && BLIND_ROLES.contains(&i.role.as_str()))
        .map(|i| (i, i.rect.intersection(window) / area))
        .filter(|(_, share)| *share >= BLIND_SHARE)
        .max_by(|a, b| a.1.total_cmp(&b.1));
    if let Some((item, share)) = blind {
        out.reason = Some(SparseReason::BlindRegion {
            role: item.role.clone(),
            share,
        });
    } else if actionable < MIN_ACTIONABLE {
        out.reason = Some(SparseReason::TooFew { actionable });
    } else {
        let needed = (area / AREA_PER_ACTIONABLE).floor() as usize;
        if actionable < needed {
            out.reason = Some(SparseReason::LowDensity { actionable, needed });
        }
    }
    out
}

fn rect_of(n: &UiNode) -> Option<Rect> {
    let b = n.bounds?;
    (b.w > 0.0 && b.h > 0.0 && b.x.is_finite() && b.y.is_finite())
        .then(|| Rect::new(b.x, b.y, b.w, b.h))
}

/// Roles whose text is the tree's own description of the screen, with no ref.
fn is_text_role(role: &str) -> bool {
    matches!(
        role,
        "statictext" | "text" | "heading" | "label" | "paragraph"
    )
}

/// Roles that are interactive but are drawing surfaces, not things with text:
/// a box on one is a mark, not an attachment to the surface.
fn is_surface_role(role: &str) -> bool {
    role.starts_with("canvas")
}

/// What the tree holds that the join and the sparsity rule need: the elements
/// to attach to, and every element with an area. `refs` maps a backend node id
/// to the ref the flattener gave it.
pub fn collect(
    root: &UiNode,
    refs: &std::collections::HashMap<u64, String>,
) -> (Vec<AxItem>, Vec<SparseItem>) {
    let mut ax = Vec::new();
    let mut sparse = Vec::new();
    fn walk(
        n: &UiNode,
        refs: &std::collections::HashMap<u64, String>,
        ax: &mut Vec<AxItem>,
        sparse: &mut Vec<SparseItem>,
    ) {
        let role = mcp_a11y::normalize_role(&n.role);
        if let Some(rect) = rect_of(n) {
            let reference = n.node_id.and_then(|id| refs.get(&id)).cloned();
            sparse.push(SparseItem {
                role: role.clone(),
                rect,
                actionable: reference.is_some(),
                children: n.children.len(),
            });
            // The value of a secure field is never read here; the field's
            // rectangle is kept so text drawn on it can be withheld.
            let text: Vec<String> = if n.is_secure() {
                Vec::new()
            } else {
                [n.name.as_deref(), n.value.as_deref()]
                    .into_iter()
                    .flatten()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            };
            if n.is_secure() {
                ax.push(AxItem {
                    id: reference,
                    kind: AxKind::Secure,
                    rect,
                    text,
                });
            } else if reference.is_some() && !is_surface_role(&role) {
                ax.push(AxItem {
                    id: reference,
                    kind: AxKind::Control,
                    rect,
                    text,
                });
            } else if is_text_role(&role) && !text.is_empty() {
                ax.push(AxItem {
                    id: None,
                    kind: AxKind::Text,
                    rect,
                    text,
                });
            }
        }
        for c in &n.children {
            walk(c, refs, ax, sparse);
        }
    }
    walk(root, refs, &mut ax, &mut sparse);
    (ax, sparse)
}

/// Backend node id to ref, for the elements the flattener numbered.
fn ref_index(snapshot: &Snapshot) -> std::collections::HashMap<u64, String> {
    snapshot
        .elements
        .iter()
        .filter_map(|(r, info)| info.node_id.map(|id| (id, r.clone())))
        .collect()
}

/// Whether OCR runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OcrMode {
    /// When the tree is sparse for the window.
    Auto,
    Always,
    Never,
}

impl OcrMode {
    fn as_str(self) -> &'static str {
        match self {
            OcrMode::Auto => "auto",
            OcrMode::Always => "always",
            OcrMode::Never => "never",
        }
    }
}

/// `observe`'s arguments, read and checked.
#[derive(Debug, Clone, PartialEq)]
pub struct ObserveArgs {
    pub app: Option<String>,
    pub window_id: Option<u32>,
    pub detail: Option<Detail>,
    pub max_elements: usize,
    pub ocr: OcrMode,
    /// Trade accuracy for speed in the recogniser. Fast by default: the text
    /// is wanted for a position to click and a word to match, and the
    /// accurate recogniser measured 25 to 52 s wall time on a window (the
    /// helper waiting on the system, 5 s of CPU) against 0.1 s for fast.
    pub accurate: bool,
    pub marks_image: bool,
}

/// Read the arguments. Pure.
pub fn parse_args(args: &Value) -> Result<ObserveArgs, String> {
    let app = args
        .get("app")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(str::to_string);
    let window_id = match args.get("window_id") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            mcp_types::args::as_u64(v)
                .and_then(|n| u32::try_from(n).ok())
                .ok_or("'window_id' must be a non-negative integer (an index from list_windows)")?,
        ),
    };
    let detail = match args.get("detail").and_then(Value::as_str) {
        None => None,
        Some(d) => Some(Detail::parse(d).ok_or("detail must be one of: low, balanced, full")?),
    };
    let max_elements = match args.get("max_elements") {
        None | Some(Value::Null) => DEFAULT_MAX_ELEMENTS,
        Some(v) => {
            let n =
                mcp_types::args::as_u64(v).ok_or("'max_elements' must be a positive integer")?;
            if n == 0 {
                return Err("'max_elements' must be at least 1".into());
            }
            (n as usize).min(MAX_ELEMENTS_CAP)
        }
    };
    let ocr = match args.get("ocr").and_then(Value::as_str) {
        None | Some("auto") => OcrMode::Auto,
        Some("always") => OcrMode::Always,
        Some("never") => OcrMode::Never,
        Some(o) => return Err(format!("unknown ocr '{o}' (auto|always|never)")),
    };
    let accurate = match args.get("ocr_level").and_then(Value::as_str) {
        None | Some("fast") => false,
        Some("accurate") => true,
        Some(l) => return Err(format!("unknown ocr_level '{l}' (fast|accurate)")),
    };
    let marks_image = args
        .get("marks_image")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(ObserveArgs {
        app,
        window_id,
        detail,
        max_elements,
        ocr,
        accurate,
        marks_image,
    })
}

/// Whether OCR should run for a verdict. Pure.
pub fn wants_ocr(mode: OcrMode, sparsity: Option<&Sparsity>) -> bool {
    match mode {
        OcrMode::Never => false,
        OcrMode::Always => true,
        OcrMode::Auto => sparsity.is_some_and(Sparsity::sparse),
    }
}

/// The `observe` tool.
pub struct ObserveModule {
    a11y: Arc<dyn A11yBackend>,
    arena: Arc<Mutex<SnapshotArena>>,
    windows: Arc<dyn WindowBackend>,
    vision: Arc<dyn VisionBackend>,
    cfg: VisionConfig,
    max_chars: usize,
    allowed_apps: Vec<String>,
}

impl ObserveModule {
    /// `arena` is the one `get_ui_tree` and the input tools share, so a ref
    /// from `observe` resolves in `ui_action`.
    pub fn new(
        a11y: Arc<dyn A11yBackend>,
        arena: Arc<Mutex<SnapshotArena>>,
        windows: Arc<dyn WindowBackend>,
        vision: Arc<dyn VisionBackend>,
        cfg: VisionConfig,
        max_chars: usize,
    ) -> Self {
        ObserveModule {
            a11y,
            arena,
            windows,
            vision,
            cfg,
            max_chars,
            allowed_apps: Vec::new(),
        }
    }

    /// `policy.allowed_apps`. Composition root only; empty refuses every
    /// named application, as in the vision and window engines.
    pub fn with_allowed_apps(mut self, allowed: Vec<String>) -> Self {
        self.allowed_apps = allowed;
        self
    }

    fn denied(&self, app: &str) -> Envelope {
        Envelope::fail(
            "observe",
            ErrorCode::PolicyDenied,
            format!("app '{app}' is not in policy.allowed_apps"),
        )
    }

    async fn observe(&self, args: &Value) -> Envelope {
        let tool = "observe";
        let a = match parse_args(args) {
            Ok(a) => a,
            Err(m) => return Envelope::fail(tool, ErrorCode::InvalidArgs, m),
        };
        // Naming an application is addressing it: held to the list as given
        // and again as what it resolves to, before anything is read.
        if let Some(name) = &a.app {
            if !mcp_types::apps::app_allowed(&self.allowed_apps, name) {
                return self.denied(name);
            }
        }
        let target = match self.windows.describe_target(a.app.as_deref()).await {
            Ok(t) => t,
            Err(e) => return win_err(tool, e),
        };
        if a.app.is_some() && !mcp_types::apps::app_allowed(&self.allowed_apps, &target.app) {
            return self.denied(&target.app);
        }

        // The tree. Refs are installed so ui_action can use them; the app is
        // not pinned.
        let req = SnapshotRequest {
            app: a.app.clone(),
            without_pin: true,
            ..SnapshotRequest::default()
        };
        let raw = match self.a11y.snapshot(&req).await {
            Ok(r) => r,
            Err(e) => return backend_err(tool, e),
        };
        let sid = self
            .arena
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .next_id();
        let f = flatten(
            &raw.root,
            raw.app.as_deref(),
            raw.window.as_deref(),
            &sid,
            &FlattenConfig {
                max_chars: self.max_chars,
                terminal_app: raw.terminal_app,
                ..FlattenConfig::default()
            },
        );
        let refs = ref_index(&f.snapshot);
        let (ax_items, sparse_items) = collect(&raw.root, &refs);
        let (ref_count, truncated, text) = (f.ref_count, f.truncated, f.text);
        self.arena
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .install(f.snapshot);

        let mut data = json!({
            "snapshot_id": sid,
            "app": raw.app,
            "window": raw.window,
            "ref_count": ref_count,
            "truncated": truncated,
            "text": text,
            "target": raw.target.as_ref().map(|t| t.from).unwrap_or(target.from.as_str()),
            "coordinate_space": "screen",
        });
        if let Some(pid) = raw.target.as_ref().and_then(|t| t.pid) {
            data["pid"] = json!(pid);
        }
        if raw.partial {
            data["partial"] = json!(true);
            data["hint"] = json!(
                "the tree stops early (the app was slow to answer), so it is incomplete: narrow it \
                 with get_ui_tree's 'surface' or 'root', or find_elements"
            );
        }

        // Which window, and where it is.
        let windows = match self.windows.list_windows(Some(&target.app)).await {
            Ok(w) => w,
            Err(e) => return win_err(tool, e),
        };
        let wid = a.window_id.unwrap_or(0);
        let window = if windows.is_empty() {
            None
        } else {
            match window_at(&windows, &target.app, target.pid, wid) {
                Ok(w) => Some(w.clone()),
                Err(m) => return Envelope::fail(tool, ErrorCode::NotFound, m),
            }
        };
        let bounds = window
            .as_ref()
            .and_then(|w| w.bounds)
            .map(|b| Rect::new(b.x, b.y, b.w, b.h));
        if let Some(w) = &window {
            data["window_id"] = json!(w.id);
            data["window_info"] = json!({
                "title": w.title, "bounds": w.bounds, "state": w.state,
            });
        }
        let sparsity = bounds.map(|b| assess_sparsity(&b, &sparse_items));
        match &sparsity {
            Some(s) => {
                data["sparse"] = json!(s.sparse());
                data["sparsity"] = s.describe();
            }
            None => {
                data["sparse"] = json!(null);
                data["sparsity"] = json!({
                    "sparse": null,
                    "reason": "the window has no known bounds, so the tree cannot be judged against it",
                    "rule": SPARSITY_RULE,
                });
            }
        }

        // OCR and the join.
        let want = wants_ocr(a.ocr, sparsity.as_ref());
        let mut merged_marks: Vec<MarkDraw> = Vec::new();
        let mut ocr_info = json!({ "mode": a.ocr.as_str(), "ran": false });
        if want {
            match self
                .run_ocr(
                    &a,
                    &target.app,
                    wid,
                    window.as_ref().map(|w| w.state),
                    &ax_items,
                    &mut data,
                )
                .await
            {
                Ok(done) => {
                    ocr_info = done.info;
                    merged_marks = done.draws;
                }
                Err(why) => {
                    ocr_info["reason"] = json!(why);
                    data["warning"] = json!(format!(
                        "the text of the window could not be read ({why}); only the accessibility tree is here"
                    ));
                }
            }
        } else if a.ocr == OcrMode::Auto {
            ocr_info["reason"] = json!(
                "the tree is not sparse for this window, so its text was not read; pass ocr: \"always\" to read it anyway"
            );
        }
        data["ocr"] = ocr_info;

        if a.marks_image {
            return self
                .with_marks_image(a, &target.app, wid, merged_marks, data)
                .await;
        }
        Envelope::ok(tool, data)
    }

    /// Read the window's text and join it to the tree. `data` receives the
    /// attachments, marks and the cover warning.
    async fn run_ocr(
        &self,
        a: &ObserveArgs,
        app: &str,
        wid: u32,
        state: Option<WindowState>,
        ax: &[AxItem],
        data: &mut Value,
    ) -> Result<OcrDone, String> {
        match state {
            None => return Err("the application has no window to read".into()),
            Some(WindowState::Minimized) => {
                return Err("the window is minimised, so it is not drawn anywhere".into())
            }
            Some(WindowState::Offscreen) => {
                return Err(
                    "the window is not on screen (another Space, or the app is hidden)".into(),
                )
            }
            Some(WindowState::Onscreen) => {}
        }
        let opts = OcrOpts {
            languages: Vec::new(),
            fast: !a.accurate,
            min_confidence: 0.3,
        };
        let res = self
            .vision
            .ocr(
                OcrTarget::Window {
                    id: wid,
                    app: Some(app.to_string()),
                },
                &opts,
            )
            .await
            .map_err(|e| match e {
                VisionError::PermissionDenied(m)
                | VisionError::NotFound(m)
                | VisionError::Unsupported(m)
                | VisionError::Failed(m) => m,
            })?;
        // The reading came from the application the name resolved to; hold it
        // to the list once more, as the vision tools do for a named app.
        if let Some(w) = &res.window {
            if a.app.is_some() && !mcp_types::apps::app_allowed(&self.allowed_apps, &w.app) {
                return Err(format!("app '{}' is not in policy.allowed_apps", w.app));
            }
            w.add_cover_to(data);
        }
        let (sx, sy) = (res.scale_x(), res.scale_y());
        let boxes: Vec<Candidate> = res
            .lines
            .iter()
            .map(|l| Candidate {
                text: l.text.clone(),
                rect: Rect::new(
                    res.origin.0 + l.px.0 * sx,
                    res.origin.1 + l.px.1 * sy,
                    l.px.2 * sx,
                    l.px.3 * sy,
                ),
                confidence: l.confidence,
            })
            .collect();
        // Where the geometry says which windows cover this one, their text is
        // left out; where it only says that something does, every mark is
        // flagged.
        let covering = res
            .window
            .as_ref()
            .map(|w| w.covered_by.as_slice())
            .unwrap_or(&[]);
        let warned = res.window.as_ref().is_some_and(|w| w.warning.is_some());
        let occluders: Vec<Rect> = covering
            .iter()
            .flat_map(|c| c.rects.iter().copied())
            .collect();
        let merged = merge(
            ax,
            &[BoxSource {
                name: "ocr".into(),
                boxes,
            }],
            &MergeOpts {
                unreliable: warned && occluders.is_empty(),
                occluders,
                max_listed: a.max_elements,
                ..MergeOpts::default()
            },
        );
        data["attached"] = json!(merged
            .attached
            .iter()
            .map(|t| json!({
                "ref": t.id, "text": t.text, "confidence": round3(t.confidence),
                "bounds": {"x": t.rect.x, "y": t.rect.y, "w": t.rect.w, "h": t.rect.h},
                "source": t.source, "same_as_ax": t.same_as_ax,
            }))
            .collect::<Vec<_>>());
        data["marks"] = json!(merged
            .marks
            .iter()
            .map(|m| {
                let (cx, cy) = m.rect.center();
                let mut v = json!({
                    "id": m.id, "text": m.text, "confidence": round3(m.confidence),
                    // The point mouse_action takes, and the box it came from.
                    "x": cx, "y": cy,
                    "bounds": {"x": m.rect.x, "y": m.rect.y, "w": m.rect.w, "h": m.rect.h},
                    "source": m.source,
                });
                if m.unreliable {
                    v["unreliable"] = json!(true);
                }
                v
            })
            .collect::<Vec<_>>());
        data["merge"] = json!(merged.stats);
        if merged.marks.iter().any(|m| m.unreliable) {
            data["marks_note"] = json!(
                "another application covers part of this window and the text was read off the screen, \
                 so a mark may belong to that application: check one before clicking it"
            );
        }
        let draws = merged
            .marks
            .iter()
            .map(|m| MarkDraw {
                label: m.id.clone(),
                rect: m.rect,
            })
            .collect();
        Ok(OcrDone {
            info: json!({
                "mode": a.ocr.as_str(), "ran": true, "via": res.via, "lines": res.lines.len(),
                "level": if a.accurate { "accurate" } else { "fast" },
                "image": {"width": res.width, "height": res.height},
            }),
            draws,
        })
    }

    /// The window's screenshot with the marks numbered on it, within the same
    /// size budget as `capture_window`.
    async fn with_marks_image(
        &self,
        a: ObserveArgs,
        app: &str,
        wid: u32,
        draws: Vec<MarkDraw>,
        mut data: Value,
    ) -> Envelope {
        let tool = "observe";
        let max_edge = self
            .cfg
            .max_edge(a.detail.unwrap_or(self.cfg.default_detail));
        let cap = match self
            .vision
            .capture_window(
                wid,
                Some(app),
                CaptureOpts {
                    max_edge: Some(max_edge),
                },
            )
            .await
        {
            Ok(c) => c,
            Err(e) => {
                let why = match e {
                    VisionError::PermissionDenied(m)
                    | VisionError::NotFound(m)
                    | VisionError::Unsupported(m)
                    | VisionError::Failed(m) => m,
                };
                data["marks_image"] = json!({ "included": false, "reason": why });
                return Envelope::ok(tool, data);
            }
        };
        if let Some(w) = &cap.window {
            if a.app.is_some() && !mcp_types::apps::app_allowed(&self.allowed_apps, &w.app) {
                return self.denied(&w.app);
            }
            w.add_cover_to(&mut data);
        }
        let to_draw: Vec<MarkDraw> = draws
            .iter()
            .map(|d| MarkDraw {
                label: d.label.clone(),
                rect: screen_to_image(
                    &d.rect,
                    cap.origin,
                    cap.screen_size,
                    (cap.width, cap.height),
                ),
            })
            .collect();
        let base64 = if to_draw.is_empty() {
            cap.base64.clone()
        } else {
            match draw_marks_b64(&cap.base64, &to_draw) {
                Ok(b) => b,
                Err(e) => {
                    data["marks_image"] = json!({ "included": false, "reason": format!("drawing the marks failed: {e}") });
                    return Envelope::ok(tool, data);
                }
            }
        };
        data["marks_image"] = json!({
            "included": true,
            "marks_drawn": to_draw.len(),
            "via": cap.via,
            "width": cap.width,
            "height": cap.height,
            "image_tokens": self.cfg.image_tokens(cap.width, cap.height),
            "coordinate_mapping": {
                "origin_x": cap.origin.0, "origin_y": cap.origin.1,
                "scale_x": cap.scale_x(), "scale_y": cap.scale_y(),
                "note": "screen_x = origin_x + image_x * scale_x; marks carry screen coordinates already",
            },
        });
        Envelope::ok_image(
            tool,
            data,
            ImageContent {
                mime_type: cap.mime_type,
                base64,
            },
        )
    }
}

struct OcrDone {
    info: Value,
    draws: Vec<MarkDraw>,
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

fn win_err(tool: &str, e: WindowError) -> Envelope {
    let (code, msg) = match e {
        WindowError::PermissionDenied(m) => (ErrorCode::PermDenied, m),
        WindowError::NotFound(m) => (ErrorCode::NotFound, m),
        WindowError::Unsupported(m) => (ErrorCode::UnsupportedOs, m),
        WindowError::Failed(m) => (ErrorCode::ActionFailed, m),
        WindowError::Refused(m) => (ErrorCode::PolicyDenied, m),
    };
    Envelope::fail(tool, code, msg)
}

fn backend_err(tool: &str, e: BackendError) -> Envelope {
    let (code, msg) = match e {
        BackendError::PermissionDenied(m) => (ErrorCode::PermDenied, m),
        BackendError::NotFound(m) => (ErrorCode::NotFound, m),
        BackendError::Unsupported(m) => (ErrorCode::UnsupportedOs, m),
        BackendError::Failed(m) => (ErrorCode::ActionFailed, m),
    };
    Envelope::fail(tool, code, msg)
}

#[async_trait]
impl ToolModule for ObserveModule {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        vec![ToolDescriptor::new(
            "observe",
            Category::Vision,
            Tier::Read,
            "Everything needed to act on a desktop window, in one call: the accessibility tree (the same \
             refs as get_ui_tree, usable by ui_action) and, when the tree is sparse for that window, the \
             window's text read off its pixels and joined to the tree. Text on an element is attached to its \
             ref; text with no element is a numbered mark (m1, m2...) in reading order with the screen point \
             and box mouse_action takes. Sparse means fewer than 5 actionable elements in the window, or \
             fewer than one per 120000 square points of it, or a childless canvas, web view or group \
             covering a quarter of it. marks_image returns the window's screenshot with the marks numbered \
             on it. Passive: pins, raises and focuses nothing, so it works on a window behind others.",
            json!({
                "type": "object",
                "properties": {
                    "app": { "type": "string", "description": "application to observe (default: the pinned one, else the frontmost)" },
                    "window_id": { "type": "integer", "description": "window of that application, as list_windows numbers it (default 0, the frontmost)" },
                    "detail": { "type": "string", "enum": ["low", "balanced", "full"], "description": "size of the marks_image (default from config)" },
                    "max_elements": { "type": "integer", "description": "most text entries (attached plus marks) to return, 1 to 300 (default 60); the rest are counted in merge.omitted" },
                    "ocr": { "type": "string", "enum": ["auto", "always", "never"], "description": "read the window's text: auto (only when the tree is sparse, the default), always, or never" },
                    "ocr_level": { "type": "string", "enum": ["fast", "accurate"], "description": "recogniser speed: fast (default) or accurate, which reads small text better and can take many seconds" },
                    "marks_image": { "type": "boolean", "description": "also return the window's screenshot with the marks numbered on it (default false)" }
                },
                "required": []
            }),
        )
        .details(
            "Result: `text` is get_ui_tree's flattened tree and `snapshot_id` its snapshot; `sparse` and `sparsity` \
             give the verdict and the figures (the rule is also in `sparsity.rule`); `ocr` says whether the text \
             was read (`ran`), how (`via`, the capture route) and, if it could not be, why. `attached` lists text \
             found on an element by ref; `same_as_ax` marks text the tree already says. `marks` lists the rest, each \
             with `x`,`y` (the point to pass to mouse_action) and `bounds`, all in screen points. `merge` counts \
             what was left out and why: duplicates, text the tree already has, text on a secure field (withheld, \
             never returned), text in an area another application covers, and entries over `max_elements`. When \
             another application's window covers this one and the text was read off the screen, `warning` and \
             `covered_by` say so as capture_window does; text from the covering windows' areas is left out where \
             the geometry is known and otherwise every mark carries `unreliable: true`. Text is read with the \
             platform recogniser (Apple Vision on macOS, the bundled model on Linux); a future local grounding \
             model would add boxes to the same join. The tree is of the whole application, as get_ui_tree's is; \
             `window_id` chooses which window is read for text and judged for sparsity. Refs from observe replace \
             those of the last get_ui_tree. Output carries text from the application's own interface and is \
             untrusted.",
        )
        .untrusted_output()]
    }

    async fn call(&self, name: &str, args: Value, _ctx: &CallCtx) -> Envelope {
        match name {
            "observe" => self.observe(&args).await,
            other => Envelope::fail(other, ErrorCode::InvalidArgs, "unknown tool"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_a11y::Bounds;
    use std::collections::HashMap;

    fn r(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect::new(x, y, w, h)
    }

    fn item(role: &str, rect: Rect, actionable: bool, children: usize) -> SparseItem {
        SparseItem {
            role: role.into(),
            rect,
            actionable,
            children,
        }
    }

    fn buttons(n: usize, win: &Rect) -> Vec<SparseItem> {
        (0..n)
            .map(|i| {
                item(
                    "button",
                    r(win.x + 10.0 + i as f64 * 30.0, win.y + 10.0, 24.0, 20.0),
                    true,
                    0,
                )
            })
            .collect()
    }

    #[test]
    fn a_window_with_enough_controls_is_not_sparse() {
        let win = r(100.0, 100.0, 800.0, 600.0);
        let s = assess_sparsity(&win, &buttons(12, &win));
        assert!(!s.sparse(), "{s:?}");
        assert_eq!(s.actionable, 12);
    }

    #[test]
    fn four_actionable_elements_are_too_few_whatever_the_size() {
        let win = r(0.0, 0.0, 200.0, 100.0);
        let s = assess_sparsity(&win, &buttons(4, &win));
        assert_eq!(s.reason, Some(SparseReason::TooFew { actionable: 4 }));
        // Five is enough for a small window.
        assert!(!assess_sparsity(&win, &buttons(5, &win)).sparse());
    }

    #[test]
    fn a_big_window_needs_more_controls_than_a_small_one() {
        let win = r(0.0, 0.0, 1400.0, 900.0);
        let s = assess_sparsity(&win, &buttons(6, &win));
        assert_eq!(
            s.reason,
            Some(SparseReason::LowDensity {
                actionable: 6,
                needed: 10
            })
        );
        assert!(!assess_sparsity(&win, &buttons(10, &win)).sparse());
    }

    #[test]
    fn a_large_childless_web_view_blinds_the_tree_even_with_chrome_around_it() {
        let win = r(0.0, 0.0, 1000.0, 800.0);
        let mut items = buttons(20, &win);
        items.push(item("webarea", r(0.0, 80.0, 1000.0, 700.0), false, 0));
        let s = assess_sparsity(&win, &items);
        match s.reason {
            Some(SparseReason::BlindRegion { role, share }) => {
                assert_eq!(role, "webarea");
                assert!(share > 0.8, "{share}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_web_view_with_children_is_a_described_region() {
        let win = r(0.0, 0.0, 1000.0, 800.0);
        let mut items = buttons(20, &win);
        items.push(item("webarea", r(0.0, 80.0, 1000.0, 700.0), false, 14));
        assert!(!assess_sparsity(&win, &items).sparse());
    }

    #[test]
    fn a_small_childless_group_is_decoration_not_blindness() {
        let win = r(0.0, 0.0, 1000.0, 800.0);
        let mut items = buttons(20, &win);
        items.push(item("group", r(0.0, 0.0, 200.0, 100.0), false, 0));
        assert!(!assess_sparsity(&win, &items).sparse());
    }

    #[test]
    fn only_elements_in_the_window_count() {
        // The menu bar and another window's controls are in the snapshot of
        // the application but not in this window.
        let win = r(500.0, 300.0, 400.0, 300.0);
        let other = r(0.0, 0.0, 300.0, 200.0);
        let mut items = buttons(3, &win);
        items.extend(buttons(30, &other));
        let s = assess_sparsity(&win, &items);
        assert_eq!(s.actionable, 3);
        assert!(s.sparse());
    }

    #[test]
    fn a_window_with_no_area_is_not_judged() {
        let s = assess_sparsity(&r(0.0, 0.0, 0.0, 0.0), &[]);
        assert!(!s.sparse());
    }

    #[test]
    fn auto_reads_text_only_for_a_sparse_tree() {
        let sparse = Sparsity {
            reason: Some(SparseReason::TooFew { actionable: 0 }),
            actionable: 0,
            window_area: 1.0,
        };
        let dense = Sparsity {
            reason: None,
            actionable: 50,
            window_area: 1.0,
        };
        assert!(wants_ocr(OcrMode::Auto, Some(&sparse)));
        assert!(!wants_ocr(OcrMode::Auto, Some(&dense)));
        assert!(!wants_ocr(OcrMode::Auto, None));
        assert!(wants_ocr(OcrMode::Always, Some(&dense)));
        assert!(wants_ocr(OcrMode::Always, None));
        assert!(!wants_ocr(OcrMode::Never, Some(&sparse)));
    }

    fn node(role: &str, id: Option<u64>, b: Option<(f64, f64, f64, f64)>) -> UiNode {
        UiNode {
            role: role.into(),
            node_id: id,
            bounds: b.map(|(x, y, w, h)| Bounds { x, y, w, h }),
            ..UiNode::default()
        }
    }

    #[test]
    fn the_tree_becomes_controls_text_secure_fields_and_sparsity_items() {
        let mut area = node("AXTextArea", Some(2), Some((10.0, 40.0, 500.0, 300.0)));
        area.value = Some("hello".into());
        let mut label = node("AXStaticText", None, Some((10.0, 10.0, 80.0, 18.0)));
        label.name = Some("Title".into());
        let mut secret = node("AXTextField", Some(3), Some((10.0, 350.0, 200.0, 24.0)));
        secret.subrole = Some("AXSecureTextField".into());
        secret.value = Some("hunter2".into());
        let canvas = node("canvas", Some(4), Some((0.0, 0.0, 600.0, 400.0)));
        let unbounded = node("AXButton", Some(5), None);
        let mut root = node("AXApplication", None, None);
        root.children = vec![area, label, secret, canvas, unbounded];
        let refs: HashMap<u64, String> = [(2, "@e1"), (3, "@e2"), (4, "@e3"), (5, "@e4")]
            .into_iter()
            .map(|(k, v)| (k, v.to_string()))
            .collect();
        let (ax, sparse) = collect(&root, &refs);

        let kinds: Vec<(Option<&str>, AxKind)> =
            ax.iter().map(|a| (a.id.as_deref(), a.kind)).collect();
        assert_eq!(
            kinds,
            vec![
                (Some("@e1"), AxKind::Control),
                (None, AxKind::Text),
                (Some("@e2"), AxKind::Secure),
            ]
        );
        assert_eq!(ax[0].text, vec!["hello".to_string()]);
        // The secure field's value never reaches the join.
        assert!(ax[2].text.is_empty());
        // A drawing surface is not an element to attach text to, but it counts
        // for sparsity; an element without bounds counts for neither.
        assert!(sparse.iter().any(|s| s.role == "canvas" && s.actionable));
        assert_eq!(sparse.len(), 4);
    }

    #[test]
    fn arguments_are_read_with_defaults_and_bad_ones_are_named() {
        let a = parse_args(&json!({})).unwrap();
        assert_eq!(
            a,
            ObserveArgs {
                app: None,
                window_id: None,
                detail: None,
                max_elements: 60,
                ocr: OcrMode::Auto,
                accurate: false,
                marks_image: false
            }
        );
        let a = parse_args(&json!({
            "app": " TextEdit ", "window_id": 2, "detail": "low", "max_elements": 5000,
            "ocr": "always", "marks_image": true
        }))
        .unwrap();
        assert_eq!(a.app.as_deref(), Some("TextEdit"));
        assert_eq!(a.window_id, Some(2));
        assert_eq!(a.detail, Some(Detail::Low));
        assert_eq!(a.max_elements, 300);
        assert_eq!(a.ocr, OcrMode::Always);
        assert!(a.marks_image);
        for (bad, want) in [
            (json!({"window_id": -1}), "window_id"),
            (json!({"detail": "huge"}), "detail"),
            (json!({"max_elements": 0}), "max_elements"),
            (json!({"ocr": "sometimes"}), "ocr"),
            (json!({"ocr_level": "slow"}), "ocr_level"),
        ] {
            let e = parse_args(&bad).unwrap_err();
            assert!(e.contains(want), "{bad}: {e}");
        }
        // A blank app is no app.
        assert_eq!(parse_args(&json!({"app": "  "})).unwrap().app, None);
    }
}
