use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mcp_a11y::{Bounds, RefError, SnapshotArena};
use mcp_types::{CallCtx, Category, Envelope, ErrorCode, Tier, ToolDescriptor, ToolModule};
use serde_json::{json, Value};

use crate::backend::{
    valid_combo, ClipFormat, InputBackend, InputError, MouseKind, ScrollDir, SemanticAction,
};

/// Per-action policy the input engine enforces (the tier/category gate already
/// ran in `mcp-core`; these are the input-specific checks from `planning.md`
/// §5.2/§7.2).
#[derive(Debug, Clone)]
pub struct InputPolicy {
    /// Restrict coordinate input to the bounds of the current snapshot.
    pub clamp_input: bool,
    /// App-name fragments considered terminals (destructive gate scope).
    pub terminal_apps: Vec<String>,
    /// No consent channel: destructive matches become denials.
    pub autonomous: bool,
    /// Destructive-command substrings.
    pub destructive_patterns: Vec<String>,
}

/// Apps whose keystrokes may be shell input.
///
/// Editors are in this list because their *integrated terminals* are: a
/// `rm -rf` typed into VS Code's panel runs exactly as it would in Terminal.app,
/// and the accessibility API cannot tell the editor pane from the terminal pane
/// (Electron and GPUI both render their own text). Treating the whole app as a
/// terminal over-triggers on source files that merely contain the string — the
/// safe direction to be wrong in, and it only ever asks rather than refuses.
pub fn default_terminal_apps() -> Vec<String> {
    [
        "Terminal",
        "iTerm",
        "Warp",
        "Alacritty",
        "kitty",
        "Ghostty",
        "WezTerm",
        "Hyper",
        "Tabby",
        // Editors with integrated terminals.
        "Code",
        "Visual Studio Code",
        "VSCodium",
        "Cursor",
        "Windsurf",
        "Zed",
        "Sublime Text",
        "JetBrains",
        "IntelliJ",
        "PyCharm",
        "WebStorm",
        "RustRover",
        "Xcode",
        "Nova",
        "Emacs",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Read and validate a `modifiers` array. An unrecognised name is an error
/// rather than a silent drop: a `cmd+click` that quietly became a plain
/// click would look like it worked and do the wrong thing.
#[allow(clippy::result_large_err)] // Err is a ready-to-return Envelope, short-lived
fn parse_modifiers(tool: &str, args: &Value) -> Result<Vec<String>, Envelope> {
    let Some(list) = args.get("modifiers") else {
        return Ok(Vec::new());
    };
    let Some(arr) = list.as_array() else {
        return Err(Envelope::fail(
            tool,
            ErrorCode::InvalidArgs,
            "'modifiers' must be an array of strings",
        ));
    };
    let mut out = Vec::with_capacity(arr.len());
    for m in arr {
        let name = m.as_str().ok_or_else(|| {
            Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                "'modifiers' entries must be strings",
            )
        })?;
        if !crate::backend::valid_modifier(name) {
            return Err(Envelope::fail_with(
                tool,
                ErrorCode::InvalidArgs,
                format!("unknown modifier '{name}'"),
                "use cmd, shift, opt/alt, ctrl or fn",
            ));
        }
        out.push(name.to_string());
    }
    Ok(out)
}

/// Should text typed at `target` be screened as shell input?
///
/// `None` means the destination could not be determined, and that is treated as
/// a terminal. An unknown destination is not evidence of safety — and the case
/// that produces it is exactly the dangerous one: an app that exposes no
/// accessibility tree, which is what Electron and GPUI editors do.
pub fn screens_as_terminal(target: Option<&str>, terminal_apps: &[String]) -> bool {
    let Some(app) = target else {
        return true;
    };
    let lower = app.to_ascii_lowercase();
    terminal_apps
        .iter()
        .any(|t| lower.contains(&t.to_ascii_lowercase()))
}

impl Default for InputPolicy {
    fn default() -> Self {
        InputPolicy {
            clamp_input: true,
            terminal_apps: default_terminal_apps(),
            autonomous: false,
            destructive_patterns: mcp_policy::default_destructive_patterns(),
        }
    }
}

/// The `input` engine. Shares the `mcp-a11y` arena so it can act on refs from
/// the latest `get_ui_tree`.
/// Tools that accept an `expect` clause: the ones that change the UI and whose
/// effect is therefore worth confirming in the same call.
const EXPECTING: &[&str] = &[
    "ui_action",
    "set_value",
    "keyboard_type",
    "keyboard_shortcut",
    "mouse_action",
];

pub struct InputModule {
    backend: Arc<dyn InputBackend>,
    arena: Arc<Mutex<SnapshotArena>>,
    policy: InputPolicy,
    /// Runs `expect` clauses. `None` where there is no window backend to
    /// observe with, in which case an `expect` is refused rather than ignored.
    verifier: Option<std::sync::Arc<crate::postcondition::Verifier>>,
    /// Marks the session as driving, so the human-override watcher only
    /// samples the pointer when a divergence would mean something.
    activity: Option<std::sync::Arc<crate::Activity>>,
}

impl InputModule {
    pub fn new(
        backend: Arc<dyn InputBackend>,
        arena: Arc<Mutex<SnapshotArena>>,
        policy: InputPolicy,
    ) -> Self {
        InputModule {
            backend,
            arena,
            policy,
            verifier: None,
            activity: None,
        }
    }

    /// Attach the driving-activity marker (composition root only).
    pub fn with_activity(mut self, a: std::sync::Arc<crate::Activity>) -> Self {
        self.activity = Some(a);
        self
    }

    /// Attach the postcondition verifier (composition root only).
    pub fn with_verifier(mut self, v: std::sync::Arc<crate::postcondition::Verifier>) -> Self {
        self.verifier = Some(v);
        self
    }

    // ---- shared helpers -----------------------------------------------------

    /// Resolve a ref to its backend node id and bounds from the latest snapshot.
    #[allow(clippy::result_large_err)] // Err is a ready-to-return Envelope, short-lived
    fn resolve_ref(&self, tool: &str, reff: &str) -> Result<(u64, Option<Bounds>), Envelope> {
        if !valid_ref(reff) {
            return Err(Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                "ref must match @e<number>",
            ));
        }
        let arena = self.arena.lock().unwrap_or_else(|e| e.into_inner());
        match arena.resolve_latest(reff) {
            Ok(info) => match info.node_id {
                Some(id) => Ok((id, info.bounds)),
                None => Err(Envelope::fail(
                    tool,
                    ErrorCode::ActionFailed,
                    "element is not actionable (no backend handle)",
                )),
            },
            Err(RefError::Stale) => Err(Envelope::fail_with(
                tool,
                ErrorCode::StaleRef,
                "no current snapshot for this ref",
                "call get_ui_tree first",
            )),
            Err(RefError::NotFound) => Err(Envelope::fail(
                tool,
                ErrorCode::NotFound,
                "no such element in the latest snapshot",
            )),
        }
    }

    fn current_app(&self) -> Option<String> {
        let arena = self.arena.lock().unwrap_or_else(|e| e.into_inner());
        arena.current().and_then(|s| s.app.clone())
    }

    /// The union of all element bounds in the current snapshot (the coordinate
    /// region we consider "inside the observed app").
    fn allowed_region(&self) -> Option<(f64, f64, f64, f64)> {
        let arena = self.arena.lock().unwrap_or_else(|e| e.into_inner());
        let snap = arena.current()?;
        let mut iter = snap.elements.values().filter_map(|e| e.bounds);
        let first = iter.next()?;
        let mut region = (first.x, first.y, first.x + first.w, first.y + first.h);
        for b in iter {
            region.0 = region.0.min(b.x);
            region.1 = region.1.min(b.y);
            region.2 = region.2.max(b.x + b.w);
            region.3 = region.3.max(b.y + b.h);
        }
        Some(region)
    }

    /// Deny a coordinate outside the allowed region when clamping is on. If no
    /// bounds are known, clamping can't be enforced (allowed, logged).
    fn clamp_check(&self, tool: &str, x: f64, y: f64) -> Option<Envelope> {
        if !self.policy.clamp_input {
            return None;
        }
        match self.allowed_region() {
            Some((minx, miny, maxx, maxy)) => {
                if x < minx || y < miny || x > maxx || y > maxy {
                    Some(Envelope::fail(
                        tool,
                        ErrorCode::PolicyDenied,
                        "coordinate outside allowlisted window bounds (clamp_input_to_allowed)",
                    ))
                } else {
                    None
                }
            }
            None => {
                tracing::debug!("clamp on but no element bounds in snapshot; allowing");
                None
            }
        }
    }

    /// Deny/consent destructive text typed into a terminal app.
    ///
    /// The target is whatever will actually receive the keystrokes, not the app
    /// in the last snapshot. Asking the snapshot lets a destructive command
    /// straight through whenever the two disagree — and they always disagree for
    /// an app with no accessibility tree, where the snapshot is empty and the
    /// check silently evaluates to "not a terminal".
    ///
    /// When the target cannot be determined at all, destructive text is treated
    /// as if it *were* headed for a terminal. Unknown destination is not
    /// evidence of safety.
    fn destructive_check(&self, tool: &str, text: &str) -> Option<Envelope> {
        let target = self.backend.input_target().or_else(|| self.current_app());
        let is_term = screens_as_terminal(target.as_deref(), &self.policy.terminal_apps);
        if is_term && mcp_policy::is_destructive(text, &self.policy.destructive_patterns) {
            return Some(if self.policy.autonomous {
                Envelope::fail(
                    tool,
                    ErrorCode::PolicyDenied,
                    "destructive command blocked (autonomous mode)",
                )
            } else {
                Envelope::fail_with(
                    tool,
                    ErrorCode::ConsentRequired,
                    "destructive command requires human consent",
                    "confirm interactively or run a non-destructive command",
                )
            });
        }
        None
    }

    /// Resolve a `{ref}` or `{x,y}` point spec to coordinates.
    #[allow(clippy::result_large_err)] // Err is a ready-to-return Envelope, short-lived
    fn parse_point(&self, tool: &str, spec: &Value) -> Result<(f64, f64), Envelope> {
        if let Some(reff) = spec.get("ref").and_then(Value::as_str) {
            let (_, bounds) = self.resolve_ref(tool, reff)?;
            let b = bounds.ok_or_else(|| {
                Envelope::fail(tool, ErrorCode::ActionFailed, "element has no bounds")
            })?;
            Ok((b.x + b.w / 2.0, b.y + b.h / 2.0))
        } else if let (Some(x), Some(y)) = (
            spec.get("x").and_then(Value::as_f64),
            spec.get("y").and_then(Value::as_f64),
        ) {
            Ok((x, y))
        } else {
            Err(Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                "expected {ref} or {x, y}",
            ))
        }
    }

    // ---- tools --------------------------------------------------------------

    async fn ui_action(&self, args: &Value) -> Envelope {
        let tool = "ui_action";
        let Some(reff) = args.get("ref").and_then(Value::as_str) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'ref'");
        };
        let Some(action) = args
            .get("action")
            .and_then(Value::as_str)
            .and_then(SemanticAction::parse)
        else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing or invalid 'action'");
        };
        let option = args.get("option").and_then(Value::as_str);
        let (node_id, _) = match self.resolve_ref(tool, reff) {
            Ok(v) => v,
            Err(e) => return e,
        };
        match self.backend.perform(node_id, action, option).await {
            Ok(()) => Envelope::ok(tool, json!({ "ok": true })),
            Err(e) => input_err(tool, e),
        }
    }

    async fn set_value(&self, args: &Value) -> Envelope {
        let tool = "set_value";
        let Some(reff) = args.get("ref").and_then(Value::as_str) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'ref'");
        };
        let Some(text) = args.get("text").and_then(Value::as_str) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'text'");
        };
        if let Some(deny) = self.destructive_check(tool, text) {
            return deny;
        }
        let (node_id, _) = match self.resolve_ref(tool, reff) {
            Ok(v) => v,
            Err(e) => return e,
        };
        match self.backend.set_value(node_id, text).await {
            Ok(()) => Envelope::ok(tool, json!({ "ok": true })),
            Err(e) => input_err(tool, e),
        }
    }

    async fn keyboard_type(&self, args: &Value) -> Envelope {
        let tool = "keyboard_type";
        let Some(text) = args.get("text").and_then(Value::as_str) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'text'");
        };
        if let Some(deny) = self.destructive_check(tool, text) {
            return deny;
        }
        if let Some(reff) = args.get("ref").and_then(Value::as_str) {
            let (node_id, _) = match self.resolve_ref(tool, reff) {
                Ok(v) => v,
                Err(e) => return e,
            };
            if let Err(e) = self
                .backend
                .perform(node_id, SemanticAction::Focus, None)
                .await
            {
                return input_err(tool, e);
            }
        }
        match self.backend.type_text(text).await {
            Ok(()) => Envelope::ok(tool, json!({ "ok": true, "typed": text.chars().count() })),
            Err(e) => input_err(tool, e),
        }
    }

    async fn keyboard_shortcut(&self, args: &Value) -> Envelope {
        let tool = "keyboard_shortcut";
        let Some(combo) = args.get("combo").and_then(Value::as_str) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'combo'");
        };
        if !valid_combo(combo) {
            return Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                "invalid combo (e.g. cmd+shift+n)",
            );
        }
        match self.backend.key_combo(combo).await {
            Ok(()) => Envelope::ok(tool, json!({ "ok": true })),
            Err(e) => input_err(tool, e),
        }
    }

    async fn mouse_action(&self, args: &Value) -> Envelope {
        let tool = "mouse_action";
        let Some(kind) = args
            .get("type")
            .and_then(Value::as_str)
            .and_then(MouseKind::parse)
        else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing or invalid 'type'");
        };
        let (Some(x), Some(y)) = (
            args.get("x").and_then(Value::as_f64),
            args.get("y").and_then(Value::as_f64),
        ) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'x'/'y'");
        };
        if let Some(deny) = self.clamp_check(tool, x, y) {
            return deny;
        }
        let button = args.get("button").and_then(Value::as_str);
        let modifiers = match parse_modifiers(tool, args) {
            Ok(m) => m,
            Err(e) => return e,
        };
        match self.backend.mouse(kind, x, y, button, &modifiers).await {
            Ok(()) => Envelope::ok(tool, json!({ "ok": true, "modifiers": modifiers })),
            Err(e) => input_err(tool, e),
        }
    }

    async fn scroll(&self, args: &Value) -> Envelope {
        let tool = "scroll";
        let Some(dir) = args
            .get("direction")
            .and_then(Value::as_str)
            .and_then(ScrollDir::parse)
        else {
            return Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                "missing or invalid 'direction'",
            );
        };
        let amount = args.get("amount").and_then(Value::as_i64).unwrap_or(3) as i32;
        let (x, y) = match self.parse_point(tool, args) {
            Ok(p) => p,
            Err(e) => return e,
        };
        if let Some(deny) = self.clamp_check(tool, x, y) {
            return deny;
        }
        match self.backend.scroll_at(x, y, dir, amount).await {
            Ok(()) => Envelope::ok(tool, json!({ "ok": true })),
            Err(e) => input_err(tool, e),
        }
    }

    async fn hover(&self, args: &Value) -> Envelope {
        let tool = "hover";
        let (x, y) = match self.parse_point(tool, args) {
            Ok(p) => p,
            Err(e) => return e,
        };
        if let Some(deny) = self.clamp_check(tool, x, y) {
            return deny;
        }
        match self.backend.hover(x, y).await {
            Ok(()) => Envelope::ok(tool, json!({ "ok": true })),
            Err(e) => input_err(tool, e),
        }
    }

    async fn drag_drop(&self, args: &Value) -> Envelope {
        let tool = "drag_drop";
        let (Some(from), Some(to)) = (args.get("from"), args.get("to")) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'from'/'to'");
        };
        let from = match self.parse_point(tool, from) {
            Ok(p) => p,
            Err(e) => return e,
        };
        let to = match self.parse_point(tool, to) {
            Ok(p) => p,
            Err(e) => return e,
        };
        for (x, y) in [from, to] {
            if let Some(deny) = self.clamp_check(tool, x, y) {
                return deny;
            }
        }
        let modifiers = match parse_modifiers(tool, args) {
            Ok(m) => m,
            Err(e) => return e,
        };
        // Enough intermediate moves that a drop target sees motion, capped so a
        // caller cannot turn one gesture into thousands of synthetic events.
        let steps = args
            .get("steps")
            .and_then(Value::as_u64)
            .unwrap_or(20)
            .clamp(2, 100) as u32;
        match self.backend.drag(from, to, &modifiers, steps).await {
            Ok(()) => Envelope::ok(
                tool,
                json!({ "ok": true, "steps": steps, "modifiers": modifiers }),
            ),
            Err(e) => input_err(tool, e),
        }
    }

    async fn clipboard_read(&self, args: &Value) -> Envelope {
        let tool = "clipboard_read";
        let format = args
            .get("format")
            .and_then(Value::as_str)
            .and_then(ClipFormat::parse)
            .unwrap_or(ClipFormat::Text);
        match self.backend.clipboard_read(format).await {
            Ok(clip) => Envelope::ok(
                tool,
                json!({ "format": clip.format.as_str(), "data": clip.data }),
            ),
            Err(e) => input_err(tool, e),
        }
    }

    async fn clipboard_write(&self, args: &Value) -> Envelope {
        let tool = "clipboard_write";
        let format = args
            .get("format")
            .and_then(Value::as_str)
            .and_then(ClipFormat::parse)
            .unwrap_or(ClipFormat::Text);
        let Some(data) = args.get("data").and_then(Value::as_str) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'data'");
        };
        if let Some(deny) = self.destructive_check(tool, data) {
            return deny;
        }
        match self.backend.clipboard_write(format, data).await {
            Ok(()) => Envelope::ok(tool, json!({ "ok": true })),
            Err(e) => input_err(tool, e),
        }
    }
}

/// The `expect` clause the action tools accept.
///
/// It belongs *inside* `properties`, next to the other arguments. Hoisting it
/// to the top level of the schema — which is where it started — leaves it
/// undeclared: a client reading the schema properly never learns the argument
/// exists, and the act-and-confirm round trip silently goes unused.
fn expect_schema() -> serde_json::Value {
    mcp_window::wait_schema(
        "optional: wait for this to become true after the action, and return what changed",
    )
}

#[async_trait]
impl ToolModule for InputModule {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        let obj = || json!({ "type": "object" });
        vec![
            ToolDescriptor::new(
                "ui_action",
                Category::Input,
                Tier::Standard,
                "Perform a semantic action on an element by ref (accessibility action, no cursor).",
                json!({"type":"object","properties":{
                    "ref":{"type":"string","description":"element ref @eN"},
                    "action":{"type":"string","enum":["click","double_click","right_click","focus","toggle","check","uncheck","expand","collapse","select","scroll_into_view"]},
                    "option":{"type":"string","description":"option label for select"},
                    "expect": expect_schema()
                },"required":["ref","action"]}),
            ),
            ToolDescriptor::new(
                "set_value",
                Category::Input,
                Tier::Standard,
                "Set the value of a text element by ref (accessibility SetValue).",
                json!({"type":"object","properties":{
                    "ref":{"type":"string"},"text":{"type":"string"},
                    "expect": expect_schema()},"required":["ref","text"]}),
            ).idempotent(true),
            ToolDescriptor::new(
                "keyboard_type",
                Category::Input,
                Tier::Standard,
                "Type Unicode text. If 'ref' is given, focus it first. Does not press return.",
                json!({"type":"object","properties":{
                    "text":{"type":"string"},"ref":{"type":"string","description":"optional element to focus first"},
                    "expect": expect_schema()},"required":["text"]}),
            ),
            ToolDescriptor::new(
                "keyboard_shortcut",
                Category::Input,
                Tier::Standard,
                "Press a key or chord, e.g. return, escape, cmd+s, cmd+shift+n.",
                json!({"type":"object","properties":{"combo":{"type":"string"},
                    "expect": expect_schema()},"required":["combo"]}),
            ),
            ToolDescriptor::new(
                "mouse_action",
                Category::Input,
                Tier::Standard,
                "Coordinate pointer action (screen control). 'modifiers' holds keys down for \
                 the click, e.g. [\"cmd\"] to open in a new tab or [\"shift\"] to extend a \
                 selection.",
                json!({"type":"object","properties":{
                    "type":{"type":"string","enum":["move","click","double","triple","right_click","down","up"]},
                    "x":{"type":"number"},"y":{"type":"number"},
                    "button":{"type":"string","description":"left|right|middle"},
                    "modifiers":{"type":"array","items":{"type":"string","enum":["cmd","shift","opt","alt","ctrl","fn"]}},
                    "expect": expect_schema()},"required":["type","x","y"]}),
            ),
            ToolDescriptor::new(
                "scroll",
                Category::Input,
                Tier::Standard,
                "Scroll at an element ref or a point.",
                json!({"type":"object","properties":{
                    "ref":{"type":"string"},"x":{"type":"number"},"y":{"type":"number"},
                    "direction":{"type":"string","enum":["up","down","left","right","page_up","page_down"]},
                    "amount":{"type":"integer"}},"required":["direction"]}),
            ),
            ToolDescriptor::new(
                "hover",
                Category::Input,
                Tier::Standard,
                "Move the pointer over an element ref or point (reveals tooltips/hover menus).",
                json!({"type":"object","properties":{"ref":{"type":"string"},"x":{"type":"number"},"y":{"type":"number"}},"required":[]}),
            ),
            ToolDescriptor::new(
                "drag_drop",
                Category::Input,
                Tier::Standard,
                "Press-move-release drag from one point/ref to another. The pointer travels \
                 in 'steps' intermediate moves so targets that track motion register the drag.",
                json!({"type":"object","properties":{
                    "from":obj(),"to":obj(),
                    "steps":{"type":"integer","description":"intermediate moves, 2-100 (default 20)"},
                    "modifiers":{"type":"array","items":{"type":"string","enum":["cmd","shift","opt","alt","ctrl","fn"]}}},
                    "required":["from","to"]}),
            ),
            ToolDescriptor::new(
                "clipboard_read",
                Category::Input,
                Tier::Standard,
                "Read the clipboard.",
                json!({"type":"object","properties":{"format":{"type":"string","enum":["text","html","image","files"]}},"required":[]}),
            ).untrusted_output(),
            ToolDescriptor::new(
                "clipboard_write",
                Category::Input,
                Tier::Standard,
                "Write the clipboard.",
                json!({"type":"object","properties":{"format":{"type":"string","enum":["text","html","image","files"]},"data":{"type":"string"}},"required":["data"]}),
            ).idempotent(true),
        ]
    }

    async fn call(&self, name: &str, args: Value, ctx: &CallCtx) -> Envelope {
        // Held for the whole call, including any `expect` wait: the pointer is
        // ours for that entire window, so a divergence during it is somebody
        // else's hand.
        let _driving = self.activity.as_ref().map(crate::Activity::begin);

        // An `expect` clause is parsed *before* the action: discovering the
        // expectation was malformed after clicking is too late to be useful.
        let spec = if EXPECTING.contains(&name) {
            match crate::postcondition::Verifier::parse(name, &args) {
                Ok(s) => s,
                Err(e) => return *e,
            }
        } else {
            None
        };
        if spec.is_some() && self.verifier.is_none() {
            return Envelope::fail_with(
                name,
                ErrorCode::UnsupportedOs,
                "postconditions need a UI to observe, and none is wired on this platform",
                "drop 'expect' and confirm separately with wait_for",
            );
        }
        // The comparison point has to be captured before the action, not after.
        let before = match (&spec, self.verifier.as_ref()) {
            (Some(_), Some(v)) => v.before(),
            _ => None,
        };

        let env = match name {
            "ui_action" => self.ui_action(&args).await,
            "set_value" => self.set_value(&args).await,
            "keyboard_type" => self.keyboard_type(&args).await,
            "keyboard_shortcut" => self.keyboard_shortcut(&args).await,
            "mouse_action" => self.mouse_action(&args).await,
            "scroll" => self.scroll(&args).await,
            "hover" => self.hover(&args).await,
            "drag_drop" => self.drag_drop(&args).await,
            "clipboard_read" => self.clipboard_read(&args).await,
            "clipboard_write" => self.clipboard_write(&args).await,
            other => Envelope::fail(other, ErrorCode::InvalidArgs, "unknown tool"),
        };

        match (spec, self.verifier.as_ref()) {
            (Some(spec), Some(v)) => {
                let verified = v.verify(&spec, before, ctx).await;
                crate::postcondition::attach(env, name, verified)
            }
            _ => env,
        }
    }
}

fn valid_ref(s: &str) -> bool {
    s.strip_prefix("@e")
        .map(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
        .unwrap_or(false)
}

fn input_err(tool: &str, e: InputError) -> Envelope {
    let (code, msg) = match e {
        InputError::PermissionDenied(m) => (ErrorCode::PermDenied, m),
        InputError::NotFound(m) => (ErrorCode::StaleRef, m),
        InputError::Unsupported(m) => (ErrorCode::UnsupportedOs, m),
        InputError::Failed(m) => (ErrorCode::ActionFailed, m),
    };
    Envelope::fail(tool, code, msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apps() -> Vec<String> {
        default_terminal_apps()
    }

    /// The gap this closes: an editor's integrated terminal runs the same shell
    /// as Terminal.app, so `rm -rf` typed into VS Code's panel is exactly as
    /// destructive — but the old list stopped at dedicated terminal emulators.
    #[test]
    fn editors_with_integrated_terminals_are_screened() {
        for app in [
            "Code",
            "Visual Studio Code",
            "Cursor",
            "Zed",
            "Windsurf",
            "IntelliJ IDEA",
            "PyCharm",
            "Xcode",
            "Terminal",
            "iTerm2",
            "Ghostty",
        ] {
            assert!(
                screens_as_terminal(Some(app), &apps()),
                "{app} runs a shell and must be screened"
            );
        }
        for app in ["TextEdit", "Finder", "Safari", "Preview"] {
            assert!(
                !screens_as_terminal(Some(app), &apps()),
                "{app} is not a shell host"
            );
        }
    }

    /// An unknown destination must screen as a terminal. This is the actual
    /// bypass: apps with no accessibility tree yield no target, and treating
    /// "don't know" as "safe" waves the destructive command straight through.
    #[test]
    fn unknown_target_is_screened_not_waved_through() {
        assert!(screens_as_terminal(None, &apps()));
        assert!(
            screens_as_terminal(None, &[]),
            "an empty policy must not turn the check off"
        );
    }

    /// `mcp-policy` carries its own copy of the list because it sits below the
    /// engines; the two must not drift.
    #[test]
    fn policy_default_matches_the_engine_default() {
        assert_eq!(
            mcp_policy::PolicyConfig::default().terminal_apps,
            default_terminal_apps(),
            "policy and input disagree on which apps host a shell"
        );
    }

    #[test]
    fn modifiers_parse_and_reject() {
        let ok = parse_modifiers("t", &json!({ "modifiers": ["cmd", "shift"] })).unwrap();
        assert_eq!(ok, vec!["cmd".to_string(), "shift".to_string()]);
        assert!(parse_modifiers("t", &json!({})).unwrap().is_empty());

        // A silently-dropped modifier is worse than an error: the click still
        // happens, just not the one that was asked for.
        let bad = parse_modifiers("t", &json!({ "modifiers": ["hyper"] })).unwrap_err();
        assert_eq!(bad.error.unwrap().code, ErrorCode::InvalidArgs);
        assert!(parse_modifiers("t", &json!({ "modifiers": "cmd" })).is_err());
        assert!(parse_modifiers("t", &json!({ "modifiers": [1] })).is_err());
    }
}
