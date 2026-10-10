use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mcp_a11y::matcher::{near_misses, parse_ref, rank_by_name, tied_with_best, Candidate};
use mcp_a11y::{
    flatten, A11yBackend, BackendError, Bounds, ElementInfo, FlattenConfig, RefError,
    SnapshotArena, SnapshotRequest,
};
use mcp_types::args::{as_f64, i64_arg, u64_arg};
use mcp_types::{CallCtx, Category, Envelope, ErrorCode, Tier, ToolDescriptor, ToolModule};
use serde_json::{json, Value};

use crate::backend::{
    ClipFormat, InputBackend, InputError, MouseKind, Reading, ScrollDir, SemanticAction,
};
use crate::combo::{parse_combo, Os};

/// Per-action policy the input engine enforces (the tier/category gate already
/// ran in `mcp-core`; these are the input-specific checks).
#[derive(Debug, Clone)]
pub struct InputPolicy {
    /// Restrict coordinate input to the bounds of the current snapshot.
    pub clamp_input: bool,
    /// App-name fragments considered terminals (destructive gate scope).
    pub terminal_apps: Vec<String>,
    /// No consent channel: destructive matches become denials.
    pub autonomous: bool,
    /// Bypass profile: the destructive gate is off entirely.
    pub bypass: bool,
    /// Destructive-command substrings.
    pub destructive_patterns: Vec<String>,
    /// The judge, consulted after the patterns and only able to add a flag.
    pub judge: Option<std::sync::Arc<mcp_policy::mcp_judge::Judge>>,
}

/// Apps whose keystrokes may be shell input.
///
/// Editors are in this list because their *integrated terminals* are: a
/// `rm -rf` typed into VS Code's panel runs exactly as it would in Terminal.app,
/// and the accessibility API cannot tell the editor pane from the terminal pane
/// (Electron and GPUI both render their own text). Treating the whole app as a
/// terminal over-triggers on source files that merely contain the string, the
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
        let raw = m.as_str().ok_or_else(|| {
            Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                "'modifiers' entries must be strings",
            )
        })?;
        // `Cmd` and `cmd` are the same key; the backends only know the latter.
        let lowered = raw.trim().to_lowercase();
        let name = lowered.as_str();
        if !crate::backend::valid_modifier(name) {
            return Err(Envelope::fail_with(
                tool,
                ErrorCode::InvalidArgs,
                format!("unknown modifier '{raw}'"),
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
/// a terminal. An unknown destination is not evidence of safety, and the case
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
            bypass: false,
            destructive_patterns: mcp_policy::default_destructive_patterns(),
            judge: None,
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
    "ui_fill_form",
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
    /// Takes a snapshot when a call names an element before anything has been
    /// observed. `None` where the composition root wired none, in which case
    /// a name needs a prior `get_ui_tree`.
    observer: Option<Arc<dyn A11yBackend>>,
    /// Configured cursor glide for demo/showcase movements.
    glide: Mutex<crate::glide::GlideConfig>,
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
            observer: None,
            glide: Mutex::new(crate::glide::GlideConfig::default()),
        }
    }

    /// Attach the driving-activity marker (composition root only).
    pub fn with_activity(mut self, a: std::sync::Arc<crate::Activity>) -> Self {
        self.activity = Some(a);
        self
    }

    /// Attach the source of fresh snapshots (composition root only).
    pub fn with_observer(mut self, o: Arc<dyn A11yBackend>) -> Self {
        self.observer = Some(o);
        self
    }

    /// Attach the postcondition verifier (composition root only).
    pub fn with_verifier(mut self, v: std::sync::Arc<crate::postcondition::Verifier>) -> Self {
        self.verifier = Some(v);
        self
    }

    /// Configure smooth cursor gliding for demos/recordings.
    pub fn with_glide(mut self, g: crate::glide::GlideConfig) -> Self {
        self.glide = Mutex::new(g);
        self
    }

    // ---- shared helpers -----------------------------------------------------

    /// Resolve a target spec to an element: `ref` (any spelling) or `name`
    /// with an optional `role`.
    ///
    /// A name is matched against the latest snapshot, best first (exact, then
    /// prefix, then substring; controls before other elements; then document
    /// order), and a snapshot is taken if there is none. `need` says what the
    /// caller will do with the element, so a candidate that cannot be used for
    /// it is never the one chosen.
    #[allow(clippy::result_large_err)] // Err is a ready-to-return Envelope, short-lived
    async fn resolve(&self, tool: &str, spec: &Value, need: Need) -> Result<Target, Envelope> {
        if let Some(raw) = spec.get("ref").filter(|v| !v.is_null()) {
            let Some(reff) = parse_ref(raw) else {
                return Err(Envelope::fail(
                    tool,
                    ErrorCode::InvalidArgs,
                    "ref must look like @e12 (e12 and 12 also work)",
                ));
            };
            return self.target_by_ref(tool, &reff, need);
        }
        let name = spec
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|n| !n.is_empty());
        let Some(name) = name else {
            return Err(Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                "needs a target: 'ref' (@e12) or 'name' (with an optional 'role')",
            ));
        };
        let role = spec
            .get("role")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|r| !r.is_empty());
        let have_snapshot = self
            .arena
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .current()
            .is_some();
        if !have_snapshot {
            self.observe(tool).await?;
        }
        self.target_by_name(tool, name, role, need)
    }

    #[allow(clippy::result_large_err)]
    fn target_by_ref(&self, tool: &str, reff: &str, need: Need) -> Result<Target, Envelope> {
        let arena = self.arena.lock().unwrap_or_else(|e| e.into_inner());
        match arena.resolve_latest(reff) {
            Ok(info) => Target::from_info(tool, reff, info, need, None),
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

    #[allow(clippy::result_large_err)]
    fn target_by_name(
        &self,
        tool: &str,
        name: &str,
        role: Option<&str>,
        need: Need,
    ) -> Result<Target, Envelope> {
        let arena = self.arena.lock().unwrap_or_else(|e| e.into_inner());
        let snap = arena.current().ok_or_else(|| {
            Envelope::fail_with(
                tool,
                ErrorCode::StaleRef,
                "no current snapshot to resolve elements by name",
                "call get_ui_tree first",
            )
        })?;
        let usable = |i: &ElementInfo| need.usable(i);
        let ranked = rank_by_name(snap, name, role, &usable);
        let Some(best) = ranked.first() else {
            let close = near_misses(snap, name, &usable, MAX_CANDIDATES);
            let what = match role {
                Some(r) => format!("no {r} matching name '{name}' in the current snapshot"),
                None => format!("no element matching name '{name}' in the current snapshot"),
            };
            // A sparse tree may just not hold the text, so say where to look
            // instead. Words the advice only: the model makes the OCR call.
            let ocr = mcp_a11y::ocr_fallback_hint(snap, name)
                .map(|h| format!(". {h}"))
                .unwrap_or_default();
            return Err(if close.is_empty() {
                Envelope::fail_with(
                    tool,
                    ErrorCode::NotFound,
                    what,
                    format!("call find_elements or get_ui_tree to see what is on screen{ocr}"),
                )
            } else {
                let lines: Vec<String> = close.iter().map(Candidate::line).collect();
                Envelope::fail_with_data(
                    tool,
                    ErrorCode::NotFound,
                    what,
                    format!("closest: {}{ocr}", lines.join("; ")),
                    json!({ "candidates": close }),
                )
            });
        };
        Target::from_info(
            tool,
            best.reff,
            best.info,
            need,
            Some(tied_with_best(&ranked)),
        )
    }

    /// Take a fresh snapshot of the target application and install it, for a
    /// call that names an element before anything has been observed.
    #[allow(clippy::result_large_err)]
    async fn observe(&self, tool: &str) -> Result<(), Envelope> {
        let Some(observer) = &self.observer else {
            return Err(Envelope::fail_with(
                tool,
                ErrorCode::StaleRef,
                "no current snapshot to resolve elements by name",
                "call get_ui_tree first",
            ));
        };
        let raw = observer
            .snapshot(&SnapshotRequest::default())
            .await
            .map_err(|e| {
                let (code, msg) = match e {
                    BackendError::PermissionDenied(m) => (ErrorCode::PermDenied, m),
                    BackendError::NotFound(m) => (ErrorCode::NotFound, m),
                    BackendError::Unsupported(m) => (ErrorCode::UnsupportedOs, m),
                    BackendError::Failed(m) => (ErrorCode::ActionFailed, m),
                };
                Envelope::fail(tool, code, msg)
            })?;
        let mut arena = self.arena.lock().unwrap_or_else(|e| e.into_inner());
        let sid = arena.next_id();
        // No character budget: the text is never returned, and truncating the
        // element map would drop the very element being looked for.
        let cfg = FlattenConfig {
            max_chars: usize::MAX,
            terminal_app: raw.terminal_app,
            ..FlattenConfig::default()
        };
        let f = flatten(
            &raw.root,
            raw.app.as_deref(),
            raw.window.as_deref(),
            &sid,
            &cfg,
        );
        arena.install(f.snapshot);
        Ok(())
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
    /// straight through whenever the two disagree, and they always disagree for
    /// an app with no accessibility tree, where the snapshot is empty and the
    /// check silently evaluates to "not a terminal".
    ///
    /// When the target cannot be determined at all, destructive text is treated
    /// as if it *were* headed for a terminal. Unknown destination is not
    /// evidence of safety.
    async fn destructive_check(&self, tool: &str, text: &str, secret: bool) -> Option<Envelope> {
        if self.policy.bypass {
            return None;
        }
        let target = self.backend.input_target().or_else(|| self.current_app());
        let is_term = screens_as_terminal(target.as_deref(), &self.policy.terminal_apps);
        if !is_term {
            return None;
        }
        // A secret (a password the operator handed us to type) must never be
        // sent to the remote judge; the offline pattern check still runs.
        let judge = if secret {
            None
        } else {
            self.policy.judge.as_ref()
        };
        let verdict = mcp_policy::judged_destructive(
            text,
            &self.policy.destructive_patterns,
            judge,
            &format!(
                "a terminal ({})",
                target.as_deref().unwrap_or("unknown application")
            ),
        )
        .await;
        if verdict.is_destructive() {
            let reason = verdict.reason();
            return Some(if self.policy.autonomous {
                Envelope::fail(
                    tool,
                    ErrorCode::PolicyDenied,
                    format!("destructive command blocked (autonomous mode): {reason}"),
                )
            } else {
                Envelope::fail_with(
                    tool,
                    ErrorCode::ConsentRequired,
                    format!("destructive command requires human consent: {reason}"),
                    "confirm interactively or run a non-destructive command",
                )
            });
        }
        None
    }

    /// Resolve a point spec to screen coordinates, and the element it was
    /// aimed at if any.
    ///
    /// `{ref}` or `{name}` aims at an element: at its centre, or, when `x`/`y`
    /// are given, that many points from its top-left corner (so the same
    /// numbers work whether the window is moved or not). Without a target,
    /// `x` and `y` are absolute screen coordinates.
    ///
    /// The point is not clamped here; every caller runs it through
    /// [`Self::clamp_check`], so what policy sees is what the pointer gets.
    #[allow(clippy::result_large_err)] // Err is a ready-to-return Envelope, short-lived
    async fn parse_point(
        &self,
        tool: &str,
        spec: &Value,
    ) -> Result<((f64, f64), Option<Target>), Envelope> {
        let x = number_arg(tool, spec, "x")?;
        let y = number_arg(tool, spec, "y")?;
        let aimed = spec.get("ref").is_some_and(|v| !v.is_null())
            || spec
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| !n.trim().is_empty());
        if aimed {
            let target = self.resolve(tool, spec, Need::Bounds).await?;
            let b = target.bounds.ok_or_else(|| {
                Envelope::fail(tool, ErrorCode::ActionFailed, "element has no bounds")
            })?;
            let at = (b.x + x.unwrap_or(b.w / 2.0), b.y + y.unwrap_or(b.h / 2.0));
            return Ok((at, Some(target)));
        }
        match (x, y) {
            (Some(x), Some(y)) => Ok(((x, y), None)),
            _ => Err(Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                "expected 'ref' or 'name' (x/y then offset from its top-left), or both x and y",
            )),
        }
    }

    /// Read an element from the OS until `done` accepts it or the wait runs
    /// out, returning the last reading.
    ///
    /// Apps apply a change on their own run loop, so the first read after an
    /// action is often the old state. A backend that cannot read at all is not
    /// polled: nothing it says later will be different.
    async fn settle(&self, node_id: u64, done: impl Fn(&Reading) -> bool) -> Option<Reading> {
        let mut last = None;
        for attempt in 0..SETTLE_READS {
            let Ok(r) = self.backend.read_element(node_id).await else {
                break;
            };
            let unreadable = r.value.is_none() && r.checked.is_none();
            let finished = done(&r);
            last = Some(r);
            if finished || unreadable {
                break;
            }
            if attempt + 1 < SETTLE_READS {
                tokio::time::sleep(SETTLE_GAP).await;
            }
        }
        last
    }

    /// Choose an option from a popup or combo box, or from the popup an entry
    /// belongs to, and say what the control shows now.
    #[allow(clippy::result_large_err)] // Err is a ready-to-return Envelope
    async fn choose(
        &self,
        tool: &str,
        target: &Target,
        node_id: u64,
        option: Option<&str>,
    ) -> Result<Value, Envelope> {
        // An entry with no option named means "this entry": an option element
        // resolves to the control that owns it, the way an HTML option does
        // to its select.
        let entry = option.is_none() && is_entry_role(&target.role);
        let wanted = match option {
            Some(o) => o.to_string(),
            None if entry => target.name.clone().unwrap_or_default(),
            None => {
                self.backend
                    .perform(node_id, SemanticAction::Select, None)
                    .await
                    .map_err(|e| input_err(tool, e))?;
                return Ok(json!({ "ok": true }));
            }
        };
        match self.backend.choose_option(node_id, &wanted).await {
            Ok(c) => Ok(json!({
                "ok": true,
                "selected": c.selected.clone().unwrap_or_else(|| c.item.clone()),
                "changed": c.changed,
                "verified": c.selected.is_some(),
            })),
            // A menu item in a menu bar has no popup to resolve to; pressing
            // it is the whole job, as it always was.
            Err(InputError::Unsupported(_)) if entry => {
                self.backend
                    .perform(node_id, SemanticAction::Select, None)
                    .await
                    .map_err(|e| input_err(tool, e))?;
                Ok(json!({ "ok": true }))
            }
            Err(e) => Err(input_err(tool, e)),
        }
    }

    /// Check, uncheck or toggle, reading the state before (so asking for the
    /// state it already has changes nothing) and after (so a press that did
    /// nothing is an error, not an `ok`).
    #[allow(clippy::result_large_err)] // Err is a ready-to-return Envelope
    async fn set_checked(
        &self,
        tool: &str,
        node_id: u64,
        action: SemanticAction,
    ) -> Result<Value, Envelope> {
        let before = self
            .backend
            .read_element(node_id)
            .await
            .ok()
            .and_then(|r| r.checked);
        let want = match action {
            SemanticAction::Check => Some(true),
            SemanticAction::Uncheck => Some(false),
            _ => before.map(|b| !b),
        };
        if action != SemanticAction::Toggle && before.is_some() && before == want {
            return Ok(json!({ "ok": true, "checked": before, "changed": false }));
        }
        self.backend
            .perform(node_id, action, None)
            .await
            .map_err(|e| input_err(tool, e))?;
        let after = self
            .settle(node_id, |r| match want {
                Some(w) => r.checked == Some(w),
                None => r.checked.is_some() && r.checked != before,
            })
            .await
            .and_then(|r| r.checked);
        if let (Some(a), Some(w)) = (after, want) {
            if a != w {
                return Err(Envelope::fail_with(
                    tool,
                    ErrorCode::ActionFailed,
                    format!(
                        "pressed it, but it is still {}",
                        if a { "checked" } else { "unchecked" }
                    ),
                    "the control may be disabled or may need a different action; try click",
                ));
            }
        }
        Ok(json!({
            "ok": true,
            "checked": after,
            "changed": before.zip(after).map(|(b, a)| b != a),
        }))
    }

    /// One semantic action on a resolved element, with the read-backs the
    /// action calls for.
    #[allow(clippy::result_large_err)] // Err is a ready-to-return Envelope
    async fn act(
        &self,
        tool: &str,
        target: &Target,
        action: SemanticAction,
        option: Option<&str>,
    ) -> Result<Value, Envelope> {
        let node_id = target.node_id.ok_or_else(|| {
            Envelope::fail(
                tool,
                ErrorCode::ActionFailed,
                "element is not actionable (no backend handle)",
            )
        })?;
        match action {
            SemanticAction::Select => self.choose(tool, target, node_id, option).await,
            SemanticAction::Check | SemanticAction::Uncheck | SemanticAction::Toggle => {
                self.set_checked(tool, node_id, action).await
            }
            _ => {
                self.backend
                    .perform(node_id, action, option)
                    .await
                    .map_err(|e| input_err(tool, e))?;
                Ok(json!({ "ok": true }))
            }
        }
    }

    /// Write text into an element and read it back. The write is only `ok`
    /// if the field now shows something other than what it showed before, or
    /// exactly what was asked for.
    #[allow(clippy::result_large_err)] // Err is a ready-to-return Envelope
    async fn write_text(
        &self,
        tool: &str,
        target: &Target,
        node_id: u64,
        text: &str,
        secret: bool,
    ) -> Result<Value, Envelope> {
        let before = self
            .backend
            .read_element(node_id)
            .await
            .ok()
            .and_then(|r| r.value);
        self.backend
            .set_value(node_id, text)
            .await
            .map_err(|e| input_err(tool, e))?;
        let after = self
            .settle(node_id, |r| {
                r.value.as_deref() == Some(text) || r.value != before
            })
            .await
            .and_then(|r| r.value);
        judge_write(
            before.as_deref(),
            after.as_deref(),
            text,
            secret || target.secure,
        )
        .map_err(|m| {
            Envelope::fail_with(
                tool,
                ErrorCode::ActionFailed,
                m,
                "the field may be read-only or may validate input; try keyboard_type",
            )
        })
    }

    // ---- tools --------------------------------------------------------------

    async fn ui_action(&self, args: &Value) -> Envelope {
        let tool = "ui_action";
        let Some(action) = args
            .get("action")
            .and_then(Value::as_str)
            .map(snake)
            .and_then(|a| SemanticAction::parse(&a))
        else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing or invalid 'action'");
        };
        let option = args
            .get("option")
            .and_then(Value::as_str)
            .filter(|o| !o.is_empty());
        let target = match self.resolve(tool, args, Need::Handle).await {
            Ok(t) => t,
            Err(e) => return e,
        };
        match self.act(tool, &target, action, option).await {
            Ok(data) => Envelope::ok(tool, target.annotate(data)),
            Err(e) => e,
        }
    }

    async fn set_value(&self, args: &Value) -> Envelope {
        let tool = "set_value";
        let Some(text) = args.get("text").and_then(Value::as_str) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'text'");
        };
        let secret = args.get("secret").and_then(Value::as_bool).unwrap_or(false);
        if let Some(deny) = self.destructive_check(tool, text, secret).await {
            return deny;
        }
        let target = match self.resolve(tool, args, Need::Handle).await {
            Ok(t) => t,
            Err(e) => return e,
        };
        let node_id = target.node_id.unwrap_or_default();
        match self.write_text(tool, &target, node_id, text, secret).await {
            Ok(data) => Envelope::ok(tool, target.annotate(data)),
            Err(e) => e,
        }
    }

    async fn ui_fill_form(&self, args: &Value) -> Envelope {
        let tool = "ui_fill_form";
        let Some(fields_arr) = args.get("fields").and_then(Value::as_array) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'fields' array");
        };
        if fields_arr.is_empty() {
            return Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                "'fields' array must not be empty",
            );
        }

        let mut results: Vec<Value> = Vec::with_capacity(fields_arr.len());

        for field in fields_arr {
            // What went wrong, with what had been done by then: a form half
            // filled is a state the caller needs to see.
            let stop = |filled: usize, results: &[Value], err: Envelope| Envelope {
                ok: false,
                tool: tool.into(),
                data: Some(merge(
                    err.data.clone().unwrap_or_else(|| json!({})),
                    json!({
                        "filled": filled,
                        "results": results,
                        "error_target": field.get("ref").or_else(|| field.get("name")),
                    }),
                )),
                error: err.error,
                image: None,
            };

            let target = match self.resolve(tool, field, Need::Handle).await {
                Ok(t) => t,
                Err(e) => return stop(results.len(), &results, e),
            };
            let node_id = target.node_id.unwrap_or_default();
            let secret = field
                .get("secret")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let action_str = field
                .get("action")
                .and_then(Value::as_str)
                .map(snake)
                .filter(|a| !a.is_empty());
            let option = field
                .get("option")
                .and_then(Value::as_str)
                .filter(|o| !o.is_empty());

            // (action label, value shown in the result, outcome)
            let done: Result<(String, Option<Value>, Value), Envelope> = if let Some(val) =
                field.get("value")
            {
                if let Some(b) = val.as_bool() {
                    let act = match action_str.as_deref() {
                        Some("toggle") => SemanticAction::Toggle,
                        Some("click") => SemanticAction::Click,
                        _ if b => SemanticAction::Check,
                        _ => SemanticAction::Uncheck,
                    };
                    self.act(tool, &target, act, None)
                        .await
                        .map(|d| (format!("{act:?}").to_lowercase(), Some(json!(b)), d))
                } else {
                    let text = val
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| val.to_string());
                    match action_str.as_deref() {
                        Some("select") => {
                            let opt = option.unwrap_or(&text);
                            self.act(tool, &target, SemanticAction::Select, Some(opt))
                                .await
                                .map(|d| {
                                    (
                                        "select".to_string(),
                                        None,
                                        merge(d, json!({ "option": opt })),
                                    )
                                })
                        }
                        Some(act) if SemanticAction::parse(act).is_some() => {
                            let sem = SemanticAction::parse(act).unwrap_or(SemanticAction::Click);
                            self.act(tool, &target, sem, None)
                                .await
                                .map(|d| (act.to_string(), None, d))
                        }
                        _ => {
                            if let Some(deny) = self.destructive_check(tool, &text, secret).await {
                                return stop(results.len(), &results, deny);
                            }
                            let shown = if secret {
                                format!("[REDACTED:len={}]", text.len())
                            } else {
                                text.clone()
                            };
                            self.write_text(tool, &target, node_id, &text, secret)
                                .await
                                .map(|d| ("set_value".to_string(), Some(json!(shown)), d))
                        }
                    }
                }
            } else if let Some(act) = action_str.as_deref() {
                let sem = SemanticAction::parse(act).unwrap_or(SemanticAction::Click);
                self.act(tool, &target, sem, option)
                    .await
                    .map(|d| (act.to_string(), None, d))
            } else {
                return stop(
                    results.len(),
                    &results,
                    Envelope::fail(
                        tool,
                        ErrorCode::InvalidArgs,
                        "field must provide 'value' or 'action'",
                    ),
                );
            };

            match done {
                Ok((label, value, mut data)) => {
                    // The write path reports what the field now shows under
                    // `value_after`; `value` stays what was asked for.
                    data = target.annotate(data);
                    let mut entry = json!({
                        "target": target.reff,
                        "action": label,
                        "status": "ok",
                    });
                    if let Some(v) = value {
                        entry["value"] = v;
                    }
                    if let Some(obj) = data.as_object_mut() {
                        obj.remove("ok");
                    }
                    results.push(merge(entry, data));
                }
                Err(e) => return stop(results.len(), &results, e),
            }
        }
        let filled = results.len();

        let submitted = if let Some(submit_spec) = args.get("submit") {
            let target = match self.resolve(tool, submit_spec, Need::Handle).await {
                Ok(t) => t,
                Err(e) => return e,
            };
            let act = submit_spec
                .get("action")
                .and_then(Value::as_str)
                .map(snake)
                .and_then(|a| SemanticAction::parse(&a))
                .unwrap_or(SemanticAction::Click);
            if let Err(e) = self.act(tool, &target, act, None).await {
                return e;
            }
            true
        } else {
            false
        };

        Envelope::ok(
            tool,
            json!({
                "filled": filled,
                "submitted": submitted,
                "results": results
            }),
        )
    }

    async fn keyboard_type(&self, args: &Value) -> Envelope {
        let tool = "keyboard_type";
        let Some(text) = args.get("text").and_then(Value::as_str) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'text'");
        };
        let secret = args.get("secret").and_then(Value::as_bool).unwrap_or(false);
        if let Some(deny) = self.destructive_check(tool, text, secret).await {
            return deny;
        }
        let aimed = args.get("ref").is_some_and(|v| !v.is_null())
            || args
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| !n.trim().is_empty());
        let mut target = None;
        let mut before = None;
        if aimed {
            let t = match self.resolve(tool, args, Need::Handle).await {
                Ok(t) => t,
                Err(e) => return e,
            };
            let node_id = t.node_id.unwrap_or_default();
            if let Err(e) = self
                .backend
                .perform(node_id, SemanticAction::Focus, None)
                .await
            {
                return input_err(tool, e);
            }
            before = self
                .backend
                .read_element(node_id)
                .await
                .ok()
                .and_then(|r| r.value);
            target = Some(t);
        }
        if let Err(e) = self.backend.type_text(text).await {
            return input_err(tool, e);
        }
        let typed = text.chars().count();
        let Some(t) = target else {
            return Envelope::ok(tool, json!({ "ok": true, "typed": typed }));
        };
        let hide = secret || t.secure;
        let after = if typed == 0 {
            None
        } else {
            self.settle(t.node_id.unwrap_or_default(), |r| r.value != before)
                .await
                .and_then(|r| r.value)
        };
        let mut data = json!({ "ok": true, "typed": typed });
        if let Some(a) = &after {
            let changed = before.as_deref() != Some(a.as_str());
            // Typing that left a text field exactly as it was did not land:
            // it was refused, or went to some other control.
            if !changed && is_text_role(&t.role) {
                return Envelope::fail_with(
                    tool,
                    ErrorCode::ActionFailed,
                    format!(
                        "typed {typed} characters but the field's value did not change{}",
                        if hide {
                            String::new()
                        } else {
                            format!(" (still {a:?})")
                        }
                    ),
                    "the field may be read-only or full, or focus may have moved; try set_value",
                );
            }
            data["changed"] = json!(changed);
            if !hide {
                data["value_after"] = json!(a);
            }
        }
        Envelope::ok(tool, t.annotate(data))
    }

    async fn keyboard_shortcut(&self, args: &Value) -> Envelope {
        let tool = "keyboard_shortcut";
        let Some(combo) = args.get("combo").and_then(Value::as_str) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'combo'");
        };
        let os = Os::from_platform(self.backend.platform());
        let parsed = match parse_combo(combo, os) {
            Ok(c) => c.canonical(),
            Err(msg) => {
                return Envelope::fail_with(
                    tool,
                    ErrorCode::InvalidArgs,
                    msg,
                    "e.g. cmd+shift+n, Control+A, Return, F5, cmd+plus",
                )
            }
        };
        match self.backend.key_combo(&parsed).await {
            Ok(()) => Envelope::ok(tool, json!({ "ok": true, "pressed": parsed })),
            Err(e) => input_err(tool, e),
        }
    }

    fn effective_glide(
        &self,
        tool: &str,
        args: &Value,
    ) -> Result<crate::glide::GlideConfig, Box<Envelope>> {
        let base = self.glide.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let speed_arg = args
            .get("speed")
            .and_then(Value::as_str)
            .and_then(crate::glide::GlidePreset::from_speed);
        let preset = speed_arg.unwrap_or(base.preset);
        // An explicit `glide` always wins. Only when it is absent does a
        // `speed` argument imply the answer: a real preset turns gliding on,
        // `instant` turns it off, and no argument keeps the configured state.
        let enabled = match args.get("glide").and_then(Value::as_bool) {
            Some(explicit) => explicit,
            None => match speed_arg {
                Some(crate::glide::GlidePreset::Instant) => false,
                Some(_) => true,
                None => base.enabled,
            },
        };
        let duration_ms = match u64_arg(args, "duration_ms") {
            Some(ms) => Some(
                crate::glide::check_duration_ms(ms)
                    .map_err(|m| Box::new(Envelope::fail(tool, ErrorCode::InvalidArgs, m)))?,
            ),
            None => base.duration_ms,
        };
        let steps = u64_arg(args, "steps")
            .map(|s| s.min(u64::from(u32::MAX)) as u32)
            .or(base.steps);
        Ok(crate::glide::GlideConfig {
            enabled,
            preset,
            duration_ms,
            steps,
            curvature: base.curvature,
        })
    }

    async fn mouse_action(
        &self,
        args: &Value,
        cancel: &mcp_types::CancelToken,
        since: u64,
    ) -> Envelope {
        let tool = "mouse_action";
        let Some(kind) = args
            .get("type")
            .and_then(Value::as_str)
            .map(snake)
            .and_then(|t| MouseKind::parse(&t))
        else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing or invalid 'type'");
        };
        let ((x, y), target) = match self.parse_point(tool, args).await {
            Ok(p) => p,
            Err(e) => return e,
        };
        if let Some(deny) = self.clamp_check(tool, x, y) {
            return deny;
        }
        let button = args.get("button").and_then(Value::as_str);
        let modifiers = match parse_modifiers(tool, args) {
            Ok(m) => m,
            Err(e) => return e,
        };
        let glide = match self.effective_glide(tool, args) {
            Ok(g) => g,
            Err(e) => return *e,
        };
        let waypoints = match crate::glide::execute_glide(
            &*self.backend,
            (x, y),
            &glide,
            cancel,
            since,
        )
        .await
        {
            Ok(n) => n,
            Err(e) => return input_err(tool, e),
        };
        match self.backend.mouse(kind, x, y, button, &modifiers).await {
            Ok(()) => {
                let data = json!({
                    "ok": true,
                    "at": { "x": x, "y": y },
                    "modifiers": modifiers,
                    "glided": waypoints > 0
                });
                Envelope::ok(
                    tool,
                    match target {
                        Some(t) => t.annotate(data),
                        None => data,
                    },
                )
            }
            Err(e) => input_err(tool, e),
        }
    }

    async fn scroll(&self, args: &Value) -> Envelope {
        let tool = "scroll";
        let Some(dir) = args
            .get("direction")
            .and_then(Value::as_str)
            .map(snake)
            .and_then(|d| ScrollDir::parse(&d))
        else {
            return Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                "missing or invalid 'direction'",
            );
        };
        let amount = i64_arg(args, "amount").unwrap_or(3).clamp(-10_000, 10_000) as i32;
        let modifiers = match parse_modifiers(tool, args) {
            Ok(m) => m,
            Err(e) => return e,
        };
        let ((x, y), target) = match self.parse_point(tool, args).await {
            Ok(p) => p,
            Err(e) => return e,
        };
        if let Some(deny) = self.clamp_check(tool, x, y) {
            return deny;
        }
        match self.backend.scroll_at(x, y, dir, amount, &modifiers).await {
            Ok(()) => {
                let data = json!({ "ok": true, "at": { "x": x, "y": y }, "modifiers": modifiers });
                Envelope::ok(
                    tool,
                    match target {
                        Some(t) => t.annotate(data),
                        None => data,
                    },
                )
            }
            Err(e) => input_err(tool, e),
        }
    }

    async fn hover(&self, args: &Value, cancel: &mcp_types::CancelToken, since: u64) -> Envelope {
        let tool = "hover";
        let ((x, y), target) = match self.parse_point(tool, args).await {
            Ok(p) => p,
            Err(e) => return e,
        };
        if let Some(deny) = self.clamp_check(tool, x, y) {
            return deny;
        }
        let glide = match self.effective_glide(tool, args) {
            Ok(g) => g,
            Err(e) => return *e,
        };
        let waypoints = match crate::glide::execute_glide(
            &*self.backend,
            (x, y),
            &glide,
            cancel,
            since,
        )
        .await
        {
            Ok(n) => n,
            Err(e) => return input_err(tool, e),
        };
        match self.backend.hover(x, y).await {
            Ok(()) => {
                let data = json!({ "ok": true, "at": { "x": x, "y": y }, "glided": waypoints > 0 });
                Envelope::ok(
                    tool,
                    match target {
                        Some(t) => t.annotate(data),
                        None => data,
                    },
                )
            }
            Err(e) => input_err(tool, e),
        }
    }

    async fn drag_drop(&self, args: &Value, since: u64) -> Envelope {
        let tool = "drag_drop";
        let from_spec = match endpoint(tool, args, "from") {
            Ok(s) => s,
            Err(e) => return e,
        };
        let to_spec = match endpoint(tool, args, "to") {
            Ok(s) => s,
            Err(e) => return e,
        };
        let (from, from_target) = match self.parse_point(tool, &from_spec).await {
            Ok(p) => p,
            Err(e) => return e,
        };
        let (to, to_target) = match self.parse_point(tool, &to_spec).await {
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
        let steps = u64_arg(args, "steps").unwrap_or(20).clamp(2, 100) as u32;
        let hold_ms = u64_arg(args, "hold_ms").unwrap_or(0).min(MAX_HOLD_MS);
        match self
            .backend
            .drag(from, to, &modifiers, steps, hold_ms, since)
            .await
        {
            Ok(()) => {
                let end = |p: (f64, f64), t: Option<Target>| match t {
                    Some(t) => t.annotate(json!({ "x": p.0, "y": p.1 })),
                    None => json!({ "x": p.0, "y": p.1 }),
                };
                Envelope::ok(
                    tool,
                    json!({
                        "ok": true,
                        "from": end(from, from_target),
                        "to": end(to, to_target),
                        "steps": steps,
                        "hold_ms": hold_ms,
                        "modifiers": modifiers,
                    }),
                )
            }
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
        let secret = args.get("secret").and_then(Value::as_bool).unwrap_or(false);
        if let Some(deny) = self.destructive_check(tool, data, secret).await {
            return deny;
        }
        match self.backend.clipboard_write(format, data).await {
            Ok(()) => Envelope::ok(tool, json!({ "ok": true })),
            Err(e) => input_err(tool, e),
        }
    }

    async fn input_showcase(&self, args: &Value) -> Envelope {
        let tool = "input_showcase";
        // Edited on a copy and stored only once every argument has been
        // accepted: a bad `speed` must not leave `enabled` half-applied.
        let mut g = self.glide.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(en) = args.get("enabled").and_then(Value::as_bool) {
            g.enabled = en;
            if en && matches!(g.preset, crate::glide::GlidePreset::Instant) {
                g.preset = crate::glide::GlidePreset::Demo;
            }
        }
        if let Some(spd) = args.get("speed").and_then(Value::as_str) {
            if let Some(preset) = crate::glide::GlidePreset::from_speed(spd) {
                g.preset = preset;
                g.enabled = !matches!(preset, crate::glide::GlidePreset::Instant);
            } else {
                return Envelope::fail(
                    tool,
                    ErrorCode::InvalidArgs,
                    "speed must be cinematic, demo, snappy, or instant",
                );
            }
        }
        if let Some(dur) = u64_arg(args, "duration_ms") {
            match crate::glide::check_duration_ms(dur) {
                Ok(ms) => g.duration_ms = Some(ms),
                Err(m) => return Envelope::fail(tool, ErrorCode::InvalidArgs, m),
            }
        }
        *self.glide.lock().unwrap_or_else(|e| e.into_inner()) = g.clone();
        Envelope::ok(
            tool,
            json!({
                "ok": true,
                "glide": {
                    "enabled": g.enabled,
                    "speed": g.preset.as_str(),
                    "duration_ms": g.duration(),
                    "steps": g.steps()
                }
            }),
        )
    }
}

/// How many candidates a failed name lookup offers.
const MAX_CANDIDATES: usize = 5;
/// Reads while waiting for an app to apply a change, and the gap between
/// them: about 0.4s in all, enough for a run-loop turn or two.
const SETTLE_READS: usize = 10;
const SETTLE_GAP: std::time::Duration = std::time::Duration::from_millis(40);
/// Longest a drag may hold the button before moving.
const MAX_HOLD_MS: u64 = 2_000;

/// What the caller will do with a resolved element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Need {
    /// Act on it through the accessibility API.
    Handle,
    /// Aim a pointer at it.
    Bounds,
}

impl Need {
    fn usable(self, info: &ElementInfo) -> bool {
        match self {
            Need::Handle => info.node_id.is_some(),
            Need::Bounds => info.bounds.is_some(),
        }
    }
}

/// An element a call resolved to, and how.
#[derive(Debug, Clone)]
struct Target {
    reff: String,
    node_id: Option<u64>,
    bounds: Option<Bounds>,
    role: String,
    name: Option<String>,
    secure: bool,
    /// For a lookup by name: how many elements matched as well as this one.
    matches: Option<usize>,
}

impl Target {
    #[allow(clippy::result_large_err)]
    fn from_info(
        tool: &str,
        reff: &str,
        info: &ElementInfo,
        need: Need,
        matches: Option<usize>,
    ) -> Result<Target, Envelope> {
        if !need.usable(info) {
            return Err(Envelope::fail(
                tool,
                ErrorCode::ActionFailed,
                match need {
                    Need::Handle => "element is not actionable (no backend handle)",
                    Need::Bounds => "element has no bounds",
                },
            ));
        }
        Ok(Target {
            reff: reff.to_string(),
            node_id: info.node_id,
            bounds: info.bounds,
            role: info.role.clone(),
            name: info.name.clone(),
            secure: info.secure,
            matches,
        })
    }

    /// Add `ref` (what was resolved) and, for a lookup by name, `matches` to
    /// a result, so the caller learns which element a name meant.
    fn annotate(&self, data: Value) -> Value {
        let mut extra = json!({ "ref": self.reff });
        if let Some(n) = self.matches {
            extra["matches"] = json!(n);
        }
        merge(data, extra)
    }
}

/// `base` with `extra`'s keys added; both must be objects, else `base` as is.
fn merge(mut base: Value, extra: Value) -> Value {
    if let (Some(b), Value::Object(e)) = (base.as_object_mut(), extra) {
        for (k, v) in e {
            b.insert(k, v);
        }
    }
    base
}

/// `Double-Click` and `double click` as `double_click`.
fn snake(s: &str) -> String {
    s.trim().to_lowercase().replace(['-', ' '], "_")
}

/// An option inside a popup, as opposed to the popup itself.
fn is_entry_role(role: &str) -> bool {
    matches!(
        mcp_a11y::normalize_role(role).as_str(),
        "menuitem" | "listitem"
    )
}

/// A control whose value is text the user types.
fn is_text_role(role: &str) -> bool {
    use mcp_a11y::matcher::role_matches;
    role_matches(role, "textfield") || role_matches(role, "combobox")
}

/// An optional numeric argument that must be a number when it is present.
#[allow(clippy::result_large_err)]
fn number_arg(tool: &str, args: &Value, key: &str) -> Result<Option<f64>, Envelope> {
    match args.get(key).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => as_f64(v).map(Some).ok_or_else(|| {
            Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                format!("'{key}' must be a number"),
            )
        }),
    }
}

/// One end of a drag, as a point spec: the `from`/`to` object, a bare ref
/// string, or the flat `from_ref`/`from_x`/... spelling.
#[allow(clippy::result_large_err)]
fn endpoint(tool: &str, args: &Value, side: &str) -> Result<Value, Envelope> {
    match args.get(side) {
        Some(v) if v.is_object() => return Ok(v.clone()),
        Some(Value::String(s)) => {
            // A bare string is a ref when it looks like one, else a name.
            return Ok(match parse_ref(&Value::String(s.clone())) {
                Some(r) if s.trim_start().starts_with('@') => json!({ "ref": r }),
                _ => json!({ "name": s }),
            });
        }
        Some(Value::Number(n)) => return Ok(json!({ "ref": n })),
        Some(Value::Null) | None => {}
        Some(_) => {
            return Err(Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                format!(
                    "'{side}' must be an object like {{\"ref\":\"@e3\"}} or {{\"x\":10,\"y\":20}}"
                ),
            ))
        }
    }
    let mut spec = serde_json::Map::new();
    for key in ["ref", "name", "role", "x", "y"] {
        if let Some(v) = args.get(format!("{side}_{key}")).filter(|v| !v.is_null()) {
            spec.insert(key.to_string(), v.clone());
        }
    }
    if spec.is_empty() {
        return Err(Envelope::fail(
            tool,
            ErrorCode::InvalidArgs,
            format!("missing '{side}': give {{ref}}, {{name}} or {{x, y}} (or {side}_ref, {side}_x, {side}_y)"),
        ));
    }
    Ok(Value::Object(spec))
}

/// Decide whether a write landed, from what the field showed before, what it
/// shows now, and what was asked for.
///
/// Landing is not equality: a number field turns `1000` into `1,000`, and a
/// field with an input mask shows what the mask made of it. So the only
/// rejection is the one that cannot be argued with: the value is exactly what
/// it was, and not what was asked for.
fn judge_write(
    before: Option<&str>,
    after: Option<&str>,
    wanted: &str,
    hide: bool,
) -> Result<Value, String> {
    let Some(after) = after else {
        // The platform would not say. Neither a success nor a failure can be
        // claimed, so say that.
        return Ok(json!({ "verified": false }));
    };
    let landed = after == wanted;
    let changed = before != Some(after);
    if !landed && !changed {
        return Err(if hide {
            "the field did not take the new text: its value is unchanged".to_string()
        } else {
            format!("the app kept {after:?}; the new text was not accepted")
        });
    }
    let mut data = json!({ "verified": true, "changed": changed });
    if !hide {
        data["value_after"] = json!(after);
        if !landed {
            // Reported rather than hidden: the field reformatted what was typed.
            data["reformatted"] = json!(true);
        }
    }
    Ok(data)
}

/// Docs-only text shared by every tool that takes a target element.
const TARGET_DETAILS: &str = "Target an element by `ref` (`@e12`; `e12` and `12` also work) or by `name`, with an \
    optional `role` to narrow it. A name is matched against the latest snapshot (a fresh one is taken if there is \
    none), ignoring case and extra whitespace: exact name first, then prefix, then substring; the name and the \
    current value both count; controls come before other elements; ties go to document order. The result reports \
    the `ref` it resolved to and, for a name, `matches`: how many elements matched as well as the one used. A \
    name that matches nothing is NOT_FOUND, with up to five elements that share a word with it.";

/// The properties that say which element, shared by every targeting tool.
fn target_props() -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    m.insert(
        "ref".into(),
        json!({"type":"string","description":"element ref @eN"}),
    );
    m.insert(
        "name".into(),
        json!({"type":"string","description":"element name or label, instead of ref; best match wins"}),
    );
    m.insert(
        "role".into(),
        json!({"type":"string","description":"narrow a name by role, e.g. button, popup button, text field"}),
    );
    m
}

/// An object schema with the target properties plus `extra`.
fn target_schema(extra: Value, required: &[&str]) -> Value {
    let mut props = target_props();
    if let Value::Object(e) = extra {
        props.extend(e);
    }
    let mut schema = json!({ "type": "object", "properties": props });
    if !required.is_empty() {
        schema["required"] = json!(required);
    }
    schema
}

/// One end of a drag.
fn point_schema(description: &str) -> Value {
    let mut props = target_props();
    props.insert("x".into(), json!({"type":"number"}));
    props.insert("y".into(), json!({"type":"number"}));
    json!({ "type": "object", "description": description, "properties": props })
}

/// The `expect` clause the action tools accept.
///
/// It belongs *inside* `properties`, next to the other arguments. Hoisting it
/// to the top level of the schema (which is where it started) leaves it
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
        vec![
            ToolDescriptor::new(
                "ui_action",
                Category::Input,
                Tier::Standard,
                "Perform a semantic action on an element by ref or name (accessibility action, no cursor). select with option chooses a popup or combo box entry by its text.",
                target_schema(json!({
                    "action":{"type":"string","enum":["click","double_click","right_click","focus","toggle","check","uncheck","expand","collapse","select","scroll_into_view"]},
                    "option":{"type":"string","description":"entry text for select"},
                    "expect": expect_schema()
                }), &["action"]),
            ).details(TARGET_DETAILS.to_string() + "\n\n\
                `select` with `option` opens a popup button or combo box, activates the entry whose text matches \
                (exactly, then ignoring extra whitespace, then ignoring case), and reads back what the control \
                shows: `selected` and `changed`. An option that does not exist is an error listing the options (25 \
                at most); a disabled one is an error; and a control that shows something else afterwards fails the \
                call with what it kept. Anything the call opened is closed again on failure. Pointing at a menu \
                item or list entry chooses it through the popup it belongs to. Without `option`, `select` presses \
                the element as before.\n\n\
                `check`, `uncheck` and `toggle` read the state first (checking a checked box changes nothing) and \
                again afterwards, and report `checked` and `changed`; a press that did not take is an error.\n\n\
                macOS opens a popup button with the accessibility press and presses its menu item; a combo box takes \
                the text. Linux invokes the combo box's menu or list item through AT-SPI."),
            ToolDescriptor::new(
                "set_value",
                Category::Input,
                Tier::Standard,
                "Set the value of a text element by ref or name (accessibility SetValue). This works on a background window and needs no focus. Set 'secret' for a password.",
                target_schema(json!({
                    "text":{"type":"string"},
                    "secret":{"type":"boolean","description":"the text is a password or other secret: keep it out of the audit log and never send it to the judge"},
                    "expect": expect_schema()}), &["text"]),
            ).idempotent(true).details(TARGET_DETAILS.to_string() + "\n\n\
                The element is read before and after the write. The result carries `value_after` (never for a \
                `secret` or a password field), `changed`, and `reformatted` when the field shows something other \
                than what was written (a number field turning 1000 into 1,000 is fine). A field that keeps its old \
                value and does not show the new text is an error, not an `ok`. `verified: false` means the platform \
                could not read the value back."),
            ToolDescriptor::new(
                "ui_fill_form",
                Category::Input,
                Tier::Standard,
                "Fill multiple native UI fields (text fields, checkboxes, switches, popups, radios) \
                 in one call, and optionally submit and verify postconditions. \
                 Target fields by ref (@eN) or name/label.",
                json!({
                    "type": "object",
                    "properties": {
                        "fields": {
                            "type": "array",
                            "description": "list of fields to set or toggle",
                            "items": target_schema(json!({
                                "value": { "description": "string text, boolean for checkbox/switch, or selection option" },
                                "action": {
                                    "type": "string",
                                    "enum": ["set_value", "click", "toggle", "check", "uncheck", "select", "focus"],
                                    "description": "action to perform (defaults to set_value for text, check/uncheck for boolean)"
                                },
                                "option": { "type": "string", "description": "entry text for a popup or combo box (with action select)" },
                                "secret": { "type": "boolean", "description": "password or secret: redacts value from audit logs and output" }
                            }), &[])
                        },
                        "submit": target_schema(json!({
                            "action": { "type": "string", "enum": ["click", "toggle"] }
                        }), &[]),
                        "expect": expect_schema()
                    },
                    "required": ["fields"]
                }),
            ).details(TARGET_DETAILS.to_string() + "\n\n\
                Fields are applied in order and the call stops at the first failure, reporting `filled` and the \
                `results` so far. Each result carries what that field's tool would: `value_after`, `selected`, \
                `checked` and `changed`. A field named by `name` resolves with the same ranking as `ui_action`, so \
                two fields with similar labels are told apart by exactness and then document order."),
            ToolDescriptor::new(
                "keyboard_type",
                Category::Input,
                Tier::Standard,
                "Type Unicode text into the focused window. If a target (ref or name) is given, focus it first and read its value back. Does not press return. Set 'secret' when typing a password so it is kept out of the audit log and never sent to the judge.",
                target_schema(json!({
                    "text":{"type":"string"},
                    "secret":{"type":"boolean","description":"the text is a password or other secret: keep it out of the audit log and never send it to the judge"},
                    "expect": expect_schema()}), &["text"]),
            ).details(TARGET_DETAILS.to_string() + "\n\n\
                With a target, the element's value is read before and after. The result carries `value_after` \
                (never for a `secret` or a password field) and `changed`; typing into a text field that then shows \
                the same value is an error. Without a target, the keystrokes go wherever focus is and nothing is \
                read back."),
            ToolDescriptor::new(
                "keyboard_shortcut",
                Category::Input,
                Tier::Standard,
                "Press a key or chord, e.g. Return, Escape, cmd+s, Control+A, cmd+shift+z.",
                json!({"type":"object","properties":{"combo":{"type":"string"},
                    "expect": expect_schema()},"required":["combo"]}),
            ).details("Modifiers then exactly one key, joined by `+` (or `-` when there is no `+`). Case does not \
                matter: `Control+A`, `ctrl-a` and `CTRL+a` are one chord. Modifiers: cmd/command, ctrl/control, \
                shift, alt/opt/option, fn, super/meta/win; `mod` (also `cmdorctrl`, `primary`) is Command on macOS \
                and Control on Linux, and on Linux `cmd` means Control. Keys: letters, digits, return/enter, tab, \
                space, escape/esc, delete/backspace (erases backwards), forwarddelete/del, insert, home, end, \
                pageup/page_up/pgup, pagedown/page_down/pgdn, up/down/left/right (also ArrowUp...), F1-F20, \
                punctuation as the character or its name (minus, equal, plus, comma, period, slash, backslash, \
                semicolon, quote, grave, leftbracket, rightbracket), and a literal `+` as `plus` or a trailing \
                `cmd++`. More than one key is an error, never a silent drop. The result's `pressed` is the \
                canonical chord that was sent."),
            ToolDescriptor::new(
                "mouse_action",
                Category::Input,
                Tier::Standard,
                "Coordinate pointer action. With a ref or name, x/y are offsets in points from the element's top-left (default: its centre); without one they are absolute screen coordinates. 'modifiers' holds keys down for the click, e.g. [\"cmd\"] or [\"shift\"].",
                target_schema(json!({
                    "type":{"type":"string","enum":["move","click","double","triple","right_click","down","up"]},
                    "x":{"type":"number","description":"screen x, or offset from the element's left"},
                    "y":{"type":"number","description":"screen y, or offset from the element's top"},
                    "button":{"type":"string","description":"left|right|middle"},
                    "modifiers":{"type":"array","items":{"type":"string","enum":["cmd","shift","opt","alt","ctrl","fn"]}},
                    "glide":{"type":"boolean","description":"smooth Bezier gliding to target coordinates"},
                    "speed":{"type":"string","enum":["cinematic","demo","snappy","instant"],"description":"gliding speed preset"},
                    "duration_ms":{"type":"integer","description":"custom gliding duration in milliseconds, 0-2000"},
                    "expect": expect_schema()}), &["type"]),
            ).details(TARGET_DETAILS.to_string() + "\n\n\
                `x` and `y` may be numbers or numeric strings. Every point, absolute or element-relative, is \
                checked against the allowed window bounds when `clamp_input_to_allowed` is on. The result's `at` \
                is the screen point that was used. Set `glide: true` or `speed` to glide the pointer along a \
                Bezier curve for demos."),
            ToolDescriptor::new(
                "scroll",
                Category::Input,
                Tier::Standard,
                "Scroll at an element (ref or name) or a point. 'modifiers' holds keys down for the wheel, e.g. [\"cmd\"] or [\"ctrl\"] to zoom.",
                target_schema(json!({
                    "x":{"type":"number","description":"screen x, or offset from the element's left"},
                    "y":{"type":"number","description":"screen y, or offset from the element's top"},
                    "direction":{"type":"string","enum":["up","down","left","right","page_up","page_down"]},
                    "amount":{"type":"integer","description":"wheel lines (default 3); a page is ten lines"},
                    "modifiers":{"type":"array","items":{"type":"string","enum":["cmd","shift","opt","alt","ctrl","fn"]}}}), &["direction"]),
            ).details(TARGET_DETAILS.to_string() + "\n\n\
                With a target, `x` and `y` are offsets from the element's top-left (default its centre); without \
                one they are absolute screen coordinates. `modifiers` takes the same names as `mouse_action` and \
                `drag_drop` and is held for the whole gesture: `[\"cmd\"]` (macOS) or `[\"ctrl\"]` zooms in most \
                documents, maps and canvases, `[\"shift\"]` scrolls sideways where an app binds it. The result's \
                `modifiers` is what was held."),
            ToolDescriptor::new(
                "hover",
                Category::Input,
                Tier::Standard,
                "Move the pointer over an element (ref or name) or point (reveals tooltips/hover menus). Supports smooth gliding via 'glide' or 'speed'.",
                target_schema(json!({
                    "x":{"type":"number","description":"screen x, or offset from the element's left"},
                    "y":{"type":"number","description":"screen y, or offset from the element's top"},
                    "glide":{"type":"boolean","description":"smooth Bezier gliding to target coordinates"},
                    "speed":{"type":"string","enum":["cinematic","demo","snappy","instant"],"description":"gliding speed preset"},
                    "duration_ms":{"type":"integer","description":"custom gliding duration in milliseconds, 0-2000"}}), &[]),
            ).details(TARGET_DETAILS.to_string() + "\n\n\
                With a target, `x` and `y` are offsets from the element's top-left (default its centre); without \
                one they are absolute screen coordinates."),
            ToolDescriptor::new(
                "drag_drop",
                Category::Input,
                Tier::Standard,
                "Press-move-release drag from one point/element to another. The pointer travels in 'steps' intermediate moves so targets that track motion register the drag.",
                json!({"type":"object","properties":{
                    "from": point_schema("where to press: {ref}, {name} or {x,y}; with ref/name, x/y offset from its top-left"),
                    "to": point_schema("where to release: same forms as from"),
                    "from_ref":{"type":"string"},"from_name":{"type":"string"},"from_x":{"type":"number"},"from_y":{"type":"number"},
                    "to_ref":{"type":"string"},"to_name":{"type":"string"},"to_x":{"type":"number"},"to_y":{"type":"number"},
                    "steps":{"type":"integer","description":"intermediate moves, 2-100 (default 20)"},
                    "hold_ms":{"type":"integer","description":"hold the button this long before moving, 0-2000"},
                    "modifiers":{"type":"array","items":{"type":"string","enum":["cmd","shift","opt","alt","ctrl","fn"]}}},
                    "required":[]}),
            ).details("Each end is `from`/`to` as an object (`{ref}`, `{name}` with an optional `role`, or \
                `{x, y}`), or the flat `from_ref`/`from_x`/... spelling. With a ref or name, `x` and `y` are offsets \
                in points from the element's top-left (default its centre); otherwise absolute screen coordinates. \
                Both points are checked against the allowed window bounds. `hold_ms` keeps the button down before \
                the first move, for drag sources (Finder icons, list rows) that wait to tell a drag from a click."),
            ToolDescriptor::new(
                "clipboard_read",
                Category::Input,
                Tier::Standard,
                "Read the clipboard. 'text'/'html' return the string in 'data'; \
                 'image' returns a base64 PNG in 'data'; 'files' returns a \
                 newline-separated list of file:// URIs.",
                json!({"type":"object","properties":{"format":{"type":"string","enum":["text","html","image","files"]}},"required":[]}),
            ).untrusted_output(),
            ToolDescriptor::new(
                "clipboard_write",
                Category::Input,
                Tier::Standard,
                "Write the clipboard. For 'image', 'data' is a base64 PNG; for \
                 'files', it is a newline-separated list of file:// URIs; \
                 otherwise it is the literal text. Set 'secret' if the data is \
                 sensitive.",
                json!({"type":"object","properties":{"format":{"type":"string","enum":["text","html","image","files"]},"data":{"type":"string"},"secret":{"type":"boolean","description":"the text is a password or other secret: keep it out of the audit log and never send it to the judge"}},"required":["data"]}),
            ).idempotent(true),
            ToolDescriptor::new(
                "input_showcase",
                Category::Input,
                Tier::Standard,
                "Glide the real mouse pointer along a smooth Bezier curve to each point instead of jumping there, for demos, screencasts and presentations. It only moves the system cursor: no click ripples, typing HUD or styled cursor are drawn on the desktop (browser_showcase draws those inside a browser tab).",
                json!({"type":"object","properties":{
                    "enabled":{"type":"boolean","description":"enable or disable smooth gliding"},
                    "speed":{"type":"string","enum":["cinematic","demo","snappy","instant","off"],"description":"speed preset (cinematic: 350ms, demo: 200ms, snappy: 100ms, instant: 0ms)"},
                    "duration_ms":{"type":"integer","description":"custom duration in milliseconds, 0-2000"}}}),
            ).idempotent(true),
        ]
    }

    async fn call(&self, name: &str, args: Value, ctx: &CallCtx) -> Envelope {
        // Read before anything can await: a takeover from here on is this
        // call's to honour, whenever its first pointer move happens to be.
        let since = self.backend.takeover_generation();

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
            "ui_fill_form" => self.ui_fill_form(&args).await,
            "keyboard_type" => self.keyboard_type(&args).await,
            "keyboard_shortcut" => self.keyboard_shortcut(&args).await,
            "mouse_action" => self.mouse_action(&args, &ctx.cancel, since).await,
            "scroll" => self.scroll(&args).await,
            "hover" => self.hover(&args, &ctx.cancel, since).await,
            "drag_drop" => self.drag_drop(&args, since).await,
            "clipboard_read" => self.clipboard_read(&args).await,
            "clipboard_write" => self.clipboard_write(&args).await,
            "input_showcase" => self.input_showcase(&args).await,
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

fn input_err(tool: &str, e: InputError) -> Envelope {
    let (code, msg) = match e {
        InputError::PermissionDenied(m) => (ErrorCode::PermDenied, m),
        InputError::NotFound(m) => (ErrorCode::StaleRef, m),
        InputError::Unsupported(m) => (ErrorCode::UnsupportedOs, m),
        InputError::InvalidArgs(m) => (ErrorCode::InvalidArgs, m),
        InputError::Failed(m) => (ErrorCode::ActionFailed, m),
    };
    Envelope::fail(tool, code, msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Choice, ClipData};

    fn apps() -> Vec<String> {
        default_terminal_apps()
    }

    /// The gap this closes: an editor's integrated terminal runs the same shell
    /// as Terminal.app, so `rm -rf` typed into VS Code's panel is exactly as
    /// destructive, but the old list stopped at dedicated terminal emulators.
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

    /// from, to, steps, hold_ms.
    type Drag = ((f64, f64), (f64, f64), u32, u64);

    #[derive(Default)]
    struct MockBackend {
        actions: std::sync::Mutex<Vec<(u64, SemanticAction, Option<String>)>>,
        values: std::sync::Mutex<Vec<(u64, String)>>,
        /// Every pointer move, in order: this is pacing logic under test, not a
        /// claim about what an OS does with them.
        moves: std::sync::Mutex<Vec<(f64, f64)>>,
        takeovers: std::sync::atomic::AtomicU64,
        /// Simulates a human grabbing the mouse while the call is already
        /// running: the takeover lands during the pointer read that starts a
        /// glide, after admission and before the first waypoint.
        takeover_during_pointer_read: std::sync::atomic::AtomicBool,
        /// Every key chord handed to the backend, as sent.
        keys: std::sync::Mutex<Vec<String>>,
        /// Every pointer event, in order, of any kind.
        pointer: std::sync::Mutex<Vec<(MouseKind, f64, f64)>>,
        drags: std::sync::Mutex<Vec<Drag>>,
        scrolls: std::sync::Mutex<Vec<(f64, f64, i32)>>,
        /// The modifiers handed to each scroll, in the same order as `scrolls`.
        scroll_modifiers: std::sync::Mutex<Vec<Vec<String>>>,
        typed: std::sync::Mutex<Vec<String>>,
        /// What `read_element` returns per node, one entry per read; the last
        /// one repeats. Scripts the *tool layer's* judging of a read-back, not
        /// anything an OS does.
        readings: std::sync::Mutex<std::collections::HashMap<u64, Vec<Reading>>>,
        reads: std::sync::Mutex<std::collections::HashMap<u64, usize>>,
        /// What `choose_option` answers, when it should not just succeed.
        choice: std::sync::Mutex<Option<Result<Choice, InputError>>>,
    }

    impl MockBackend {
        fn script(&self, node: u64, readings: Vec<Reading>) {
            self.readings.lock().unwrap().insert(node, readings);
        }
    }

    fn shown(v: &str) -> Reading {
        Reading {
            value: Some(v.into()),
            checked: None,
        }
    }

    fn checked(c: bool) -> Reading {
        Reading {
            value: None,
            checked: Some(c),
        }
    }

    #[async_trait]
    impl InputBackend for MockBackend {
        async fn choose_option(&self, node_id: u64, option: &str) -> Result<Choice, InputError> {
            self.actions.lock().unwrap().push((
                node_id,
                SemanticAction::Select,
                Some(option.to_string()),
            ));
            match self.choice.lock().unwrap().clone() {
                Some(r) => r,
                None => Ok(Choice {
                    item: option.to_string(),
                    selected: Some(option.to_string()),
                    changed: true,
                }),
            }
        }
        async fn read_element(&self, node_id: u64) -> Result<Reading, InputError> {
            let script = self.readings.lock().unwrap();
            let Some(list) = script.get(&node_id) else {
                return Ok(Reading::default());
            };
            let mut reads = self.reads.lock().unwrap();
            let n = reads.entry(node_id).or_insert(0);
            let r = list[(*n).min(list.len() - 1)].clone();
            *n += 1;
            Ok(r)
        }
        async fn perform(
            &self,
            node_id: u64,
            action: SemanticAction,
            option: Option<&str>,
        ) -> Result<(), InputError> {
            self.actions
                .lock()
                .unwrap()
                .push((node_id, action, option.map(|s| s.to_string())));
            Ok(())
        }
        async fn set_value(&self, node_id: u64, text: &str) -> Result<(), InputError> {
            self.values
                .lock()
                .unwrap()
                .push((node_id, text.to_string()));
            Ok(())
        }
        async fn type_text(&self, text: &str) -> Result<(), InputError> {
            self.typed.lock().unwrap().push(text.to_string());
            Ok(())
        }
        async fn key_combo(&self, combo: &str) -> Result<(), InputError> {
            self.keys.lock().unwrap().push(combo.to_string());
            Ok(())
        }
        async fn mouse(
            &self,
            kind: MouseKind,
            x: f64,
            y: f64,
            _button: Option<&str>,
            _modifiers: &[String],
        ) -> Result<(), InputError> {
            self.pointer.lock().unwrap().push((kind, x, y));
            if matches!(kind, MouseKind::Move) {
                self.moves.lock().unwrap().push((x, y));
            }
            Ok(())
        }
        async fn scroll_at(
            &self,
            x: f64,
            y: f64,
            _dir: ScrollDir,
            amount: i32,
            modifiers: &[String],
        ) -> Result<(), InputError> {
            self.scrolls.lock().unwrap().push((x, y, amount));
            self.scroll_modifiers
                .lock()
                .unwrap()
                .push(modifiers.to_vec());
            Ok(())
        }
        async fn hover(&self, _x: f64, _y: f64) -> Result<(), InputError> {
            Ok(())
        }
        async fn pointer_position(&self) -> Result<Option<(f64, f64)>, InputError> {
            if self
                .takeover_during_pointer_read
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                self.cancel_pending();
            }
            Ok(Some((0.0, 0.0)))
        }
        fn cancel_pending(&self) {
            self.takeovers
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        fn takeover_generation(&self) -> u64 {
            self.takeovers.load(std::sync::atomic::Ordering::SeqCst)
        }
        async fn drag(
            &self,
            from: (f64, f64),
            to: (f64, f64),
            _modifiers: &[String],
            steps: u32,
            hold_ms: u64,
            _since_takeover: u64,
        ) -> Result<(), InputError> {
            self.drags.lock().unwrap().push((from, to, steps, hold_ms));
            Ok(())
        }
        async fn clipboard_read(&self, format: ClipFormat) -> Result<ClipData, InputError> {
            Ok(ClipData { format, data: None })
        }
        async fn clipboard_write(
            &self,
            _format: ClipFormat,
            _data: &str,
        ) -> Result<(), InputError> {
            Ok(())
        }
        fn input_target(&self) -> Option<String> {
            Some("TestApp".into())
        }
        fn platform(&self) -> &'static str {
            "mock"
        }
    }

    fn test_module() -> (InputModule, Arc<MockBackend>, Arc<Mutex<SnapshotArena>>) {
        let backend = Arc::new(MockBackend::default());
        let arena = Arc::new(Mutex::new(SnapshotArena::new()));
        let mut snap = mcp_a11y::Snapshot {
            id: "s1".into(),
            app: Some("Settings".into()),
            window: Some("Preferences".into()),
            elements: std::collections::HashMap::new(),
            skeleton: false,
        };
        snap.elements.insert(
            "@e1".into(),
            mcp_a11y::ElementInfo {
                role: "text_field".into(),
                name: Some("Username".into()),
                value_preview: None,
                secure: false,
                bounds: None,
                node_id: Some(101),
                state: Default::default(),
                semantic_intent: None,
                bound_state: None,
            },
        );
        snap.elements.insert(
            "@e2".into(),
            mcp_a11y::ElementInfo {
                role: "secure_text_field".into(),
                name: Some("Password".into()),
                value_preview: None,
                secure: true,
                bounds: None,
                node_id: Some(102),
                state: Default::default(),
                semantic_intent: None,
                bound_state: None,
            },
        );
        snap.elements.insert(
            "@e3".into(),
            mcp_a11y::ElementInfo {
                role: "checkbox".into(),
                name: Some("Enable Notifications".into()),
                value_preview: None,
                secure: false,
                bounds: None,
                node_id: Some(103),
                state: Default::default(),
                semantic_intent: None,
                bound_state: None,
            },
        );
        snap.elements.insert(
            "@e4".into(),
            mcp_a11y::ElementInfo {
                role: "button".into(),
                name: Some("Save Changes".into()),
                value_preview: None,
                secure: false,
                bounds: None,
                node_id: Some(104),
                state: Default::default(),
                semantic_intent: None,
                bound_state: None,
            },
        );
        snap.elements.insert(
            "@e5".into(),
            mcp_a11y::ElementInfo {
                role: "combobox".into(),
                name: Some("Theme".into()),
                value_preview: None,
                secure: false,
                bounds: None,
                node_id: Some(105),
                state: Default::default(),
                semantic_intent: None,
                bound_state: None,
            },
        );
        arena.lock().unwrap().install(snap);

        let policy = InputPolicy {
            bypass: true,
            ..InputPolicy::default()
        };
        let module = InputModule::new(backend.clone(), arena.clone(), policy);
        (module, backend, arena)
    }

    #[tokio::test]
    async fn ui_fill_form_batches_inputs_and_submits() {
        let (module, backend, _) = test_module();
        let env = module
            .ui_fill_form(&json!({
                "fields": [
                    { "ref": "@e1", "value": "alice" },
                    { "ref": "@e2", "value": "secret123", "secret": true },
                    { "ref": "@e3", "value": true }
                ],
                "submit": { "ref": "@e4" }
            }))
            .await;

        assert!(env.ok, "{env:?}");
        let data = env.data.unwrap();
        assert_eq!(data["filled"], 3);
        assert_eq!(data["submitted"], true);

        let values = backend.values.lock().unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0], (101, "alice".into()));
        assert_eq!(values[1], (102, "secret123".into()));

        let actions = backend.actions.lock().unwrap();
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0], (103, SemanticAction::Check, None));
        assert_eq!(actions[1], (104, SemanticAction::Click, None));

        let res = data["results"].as_array().unwrap();
        assert_eq!(res[1]["value"], "[REDACTED:len=9]");
    }

    #[tokio::test]
    async fn ui_fill_form_resolves_fields_by_name() {
        let (module, backend, _) = test_module();
        let env = module
            .ui_fill_form(&json!({
                "fields": [
                    { "name": "Username", "value": "bob" },
                    { "name": "Notifications", "value": false, "action": "uncheck" }
                ]
            }))
            .await;

        assert!(env.ok, "{env:?}");
        let data = env.data.unwrap();
        assert_eq!(data["filled"], 2);
        assert_eq!(data["submitted"], false);

        let values = backend.values.lock().unwrap();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0], (101, "bob".into()));

        let actions = backend.actions.lock().unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0], (103, SemanticAction::Uncheck, None));
    }

    #[tokio::test]
    async fn ui_fill_form_rejects_empty_fields() {
        let (module, _, _) = test_module();
        let env = module.ui_fill_form(&json!({ "fields": [] })).await;
        assert!(!env.ok);
        assert_eq!(env.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn ui_fill_form_stops_and_reports_missing_target() {
        let (module, backend, _) = test_module();
        let env = module
            .ui_fill_form(&json!({
                "fields": [
                    { "ref": "@e1", "value": "alice" },
                    { "ref": "@e999", "value": "nonexistent" }
                ],
                "submit": { "ref": "@e4" }
            }))
            .await;

        assert!(!env.ok);
        let data = env.data.unwrap();
        assert_eq!(data["filled"], 1);

        let actions = backend.actions.lock().unwrap();
        assert_eq!(actions.len(), 0, "submission must not run if field fails");
    }

    #[tokio::test]
    async fn ui_fill_form_with_combobox_select_action() {
        let (module, backend, _) = test_module();
        let env = module
            .ui_fill_form(&json!({
                "fields": [
                    { "ref": "@e5", "action": "select", "option": "Dark Mode" }
                ]
            }))
            .await;

        assert!(env.ok, "{env:?}");
        let data = env.data.unwrap();
        assert_eq!(data["filled"], 1);
        let actions = backend.actions.lock().unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(
            actions[0],
            (105, SemanticAction::Select, Some("Dark Mode".into()))
        );
    }

    #[tokio::test]
    async fn ui_fill_form_rejects_missing_value_and_action() {
        let (module, _, _) = test_module();
        let env = module
            .ui_fill_form(&json!({
                "fields": [
                    { "ref": "@e1" }
                ]
            }))
            .await;

        assert!(!env.ok);
        assert_eq!(env.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn ui_fill_form_fails_on_missing_submit_target() {
        let (module, backend, _) = test_module();
        let env = module
            .ui_fill_form(&json!({
                "fields": [
                    { "ref": "@e1", "value": "alice" }
                ],
                "submit": { "ref": "@e999" }
            }))
            .await;

        assert!(!env.ok);
        assert_eq!(env.error.unwrap().code, ErrorCode::NotFound);
        let values = backend.values.lock().unwrap();
        assert_eq!(values.len(), 1, "field was filled before submit failed");
    }

    #[tokio::test]
    async fn ui_fill_form_verifies_expect_clause_rejected_when_malformed() {
        let (module, backend, _) = test_module();
        let ctx = CallCtx::new("t", mcp_types::CancelToken::new());
        let env = module
            .call(
                "ui_fill_form",
                json!({
                    "fields": [{ "ref": "@e1", "value": "alice" }],
                    "expect": "malformed_string"
                }),
                &ctx,
            )
            .await;

        assert!(!env.ok);
        assert_eq!(env.error.unwrap().code, ErrorCode::InvalidArgs);
        let values = backend.values.lock().unwrap();
        assert!(
            values.is_empty(),
            "action must not run if expect clause is malformed"
        );
    }

    #[tokio::test]
    async fn ui_fill_form_verifies_postcondition_unsupported_without_verifier() {
        let (module, backend, _) = test_module();
        let ctx = CallCtx::new("t", mcp_types::CancelToken::new());
        let env = module
            .call(
                "ui_fill_form",
                json!({
                    "fields": [{ "ref": "@e1", "value": "alice" }],
                    "expect": { "text": "Saved" }
                }),
                &ctx,
            )
            .await;

        assert!(!env.ok);
        assert_eq!(env.error.unwrap().code, ErrorCode::UnsupportedOs);
        let values = backend.values.lock().unwrap();
        assert!(
            values.is_empty(),
            "action must not run if verifier is missing"
        );
    }

    #[tokio::test]
    async fn test_input_showcase_toggle_and_presets() {
        let (module, _, _) = test_module();
        let ctx = CallCtx::new("t", mcp_types::CancelToken::new());

        // Initial default: disabled
        let env = module.call("input_showcase", json!({}), &ctx).await;
        assert!(env.ok);
        let data = env.data.unwrap();
        assert_eq!(data["glide"]["enabled"], false);
        assert_eq!(data["glide"]["speed"], "instant");

        // Enable with cinematic preset
        let env = module
            .call(
                "input_showcase",
                json!({ "enabled": true, "speed": "cinematic" }),
                &ctx,
            )
            .await;
        assert!(env.ok);
        let data = env.data.unwrap();
        assert_eq!(data["glide"]["enabled"], true);
        assert_eq!(data["glide"]["speed"], "cinematic");
        assert_eq!(data["glide"]["duration_ms"], 350);

        // Custom duration
        let env = module
            .call(
                "input_showcase",
                json!({ "speed": "demo", "duration_ms": 250 }),
                &ctx,
            )
            .await;
        assert!(env.ok);
        let data = env.data.unwrap();
        assert_eq!(data["glide"]["speed"], "demo");
        assert_eq!(data["glide"]["duration_ms"], 250);
    }

    #[tokio::test]
    async fn test_mouse_action_and_hover_with_glide_flag() {
        let (module, _, _) = test_module();
        let ctx = CallCtx::new("t", mcp_types::CancelToken::new());

        let env = module
            .call(
                "mouse_action",
                json!({
                    "type": "click",
                    "x": 200.0,
                    "y": 300.0,
                    "glide": true,
                    "speed": "cinematic"
                }),
                &ctx,
            )
            .await;
        assert!(env.ok);
        let data = env.data.unwrap();
        assert_eq!(data["glided"], true);

        let env_hover = module
            .call(
                "hover",
                json!({
                    "x": 400.0,
                    "y": 500.0,
                    "glide": true,
                    "speed": "demo"
                }),
                &ctx,
            )
            .await;
        assert!(env_hover.ok);
        let data_hover = env_hover.data.unwrap();
        assert_eq!(data_hover["glided"], true);
    }

    async fn move_to(module: &InputModule, extra: Value) -> Value {
        let mut args = json!({ "type": "move", "x": 200.0, "y": 300.0 });
        for (k, v) in extra.as_object().unwrap() {
            args[k] = v.clone();
        }
        let ctx = CallCtx::new("t", mcp_types::CancelToken::new());
        let env = module.call("mouse_action", args, &ctx).await;
        assert!(env.ok, "{env:?}");
        env.data.unwrap()
    }

    /// A takeover that lands after the call was admitted but before its first
    /// waypoint used to be wiped by the glide's own reset.
    #[tokio::test]
    async fn a_takeover_during_the_call_stops_the_glide_before_any_move() {
        let (module, backend, _) = test_module();
        let module = module.with_glide(crate::glide::GlideConfig::demo());
        backend
            .takeover_during_pointer_read
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let ctx = CallCtx::new("t", mcp_types::CancelToken::new());
        let env = module
            .call(
                "mouse_action",
                json!({ "type": "move", "x": 400, "y": 300 }),
                &ctx,
            )
            .await;
        assert!(!env.ok, "{env:?}");
        assert!(env.error.unwrap().message.contains("human took over"));
        assert!(backend.moves.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_excessive_glide_duration_is_refused_not_echoed() {
        let (module, backend, _) = test_module();
        let ctx = CallCtx::new("t", mcp_types::CancelToken::new());
        let env = module
            .call(
                "mouse_action",
                json!({ "type": "move", "x": 400, "y": 300, "glide": true, "duration_ms": 3_600_000 }),
                &ctx,
            )
            .await;
        assert!(!env.ok);
        assert_eq!(env.error.as_ref().unwrap().code, ErrorCode::InvalidArgs);
        assert!(env.error.unwrap().message.contains("2000"));
        assert!(backend.moves.lock().unwrap().is_empty());

        let env = module
            .call(
                "input_showcase",
                json!({ "enabled": true, "duration_ms": 2001 }),
                &ctx,
            )
            .await;
        assert!(!env.ok);
        // Refused as a whole: showcase is still off.
        let env = module.call("input_showcase", json!({}), &ctx).await;
        assert_eq!(env.data.unwrap()["glide"]["enabled"], false);

        let env = module
            .call("input_showcase", json!({ "duration_ms": 2000 }), &ctx)
            .await;
        assert!(env.ok);
    }

    #[tokio::test]
    async fn instant_move_is_one_move_and_not_glided() {
        let (module, backend, _) = test_module();
        let data = move_to(&module, json!({})).await;
        assert_eq!(data["glided"], false);
        assert_eq!(backend.moves.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn demo_move_glides_sixteen_waypoints_then_moves() {
        let (module, backend, _) = test_module();
        let module = module.with_glide(crate::glide::GlideConfig::demo());
        let data = move_to(&module, json!({})).await;
        assert_eq!(data["glided"], true);
        assert_eq!(backend.moves.lock().unwrap().len(), 17);
    }

    /// The bug: `glide:false` was overridden by a non-instant preset.
    #[tokio::test]
    async fn explicit_glide_false_wins_over_the_configured_preset() {
        let (module, backend, _) = test_module();
        let module = module.with_glide(crate::glide::GlideConfig::demo());
        let data = move_to(&module, json!({ "glide": false })).await;
        assert_eq!(data["glided"], false);
        assert_eq!(backend.moves.lock().unwrap().len(), 1);

        let data = move_to(&module, json!({ "glide": false, "speed": "cinematic" })).await;
        assert_eq!(data["glided"], false);
    }

    #[tokio::test]
    async fn glided_reports_waypoints_not_configuration() {
        let (module, backend, _) = test_module();
        // glide requested, but the preset is instant: nothing is emitted.
        let data = move_to(&module, json!({ "glide": true, "speed": "instant" })).await;
        assert_eq!(data["glided"], false);
        assert_eq!(backend.moves.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn speed_alone_turns_gliding_on() {
        let (module, _, _) = test_module();
        let data = move_to(&module, json!({ "speed": "snappy" })).await;
        assert_eq!(data["glided"], true);
    }

    // ---- keyboard_shortcut --------------------------------------------------

    async fn call(module: &InputModule, tool: &str, args: Value) -> Envelope {
        let ctx = CallCtx::new("t", mcp_types::CancelToken::new());
        module.call(tool, args, &ctx).await
    }

    /// The spellings a model reaches for first used to be refused outright.
    #[tokio::test]
    async fn shortcut_spellings_reach_the_backend_in_canonical_form() {
        let (module, backend, _) = test_module();
        for (given, sent) in [
            ("Control+A", "ctrl+a"),
            ("Cmd+Shift+Z", "cmd+shift+z"),
            ("ctrl-a", "ctrl+a"),
            ("Return", "return"),
            ("Esc", "escape"),
            ("PageUp", "pageup"),
            ("cmd++", "cmd+plus"),
            ("mod+c", "cmd+c"),
        ] {
            let env = call(&module, "keyboard_shortcut", json!({ "combo": given })).await;
            assert!(env.ok, "{given}: {env:?}");
            assert_eq!(env.data.unwrap()["pressed"], sent, "{given}");
            assert_eq!(backend.keys.lock().unwrap().last().unwrap(), sent);
        }
    }

    #[tokio::test]
    async fn two_keys_are_an_error_and_nothing_is_pressed() {
        let (module, backend, _) = test_module();
        let env = call(&module, "keyboard_shortcut", json!({ "combo": "a+b" })).await;
        assert!(!env.ok);
        let err = env.error.unwrap();
        assert_eq!(err.code, ErrorCode::InvalidArgs);
        assert!(err.message.contains("2 keys"), "{}", err.message);
        assert!(backend.keys.lock().unwrap().is_empty());

        let env = call(&module, "keyboard_shortcut", json!({ "combo": "cmd+nope" })).await;
        assert!(
            env.error.unwrap().message.contains("pageup"),
            "lists the keys"
        );
    }

    // ---- targeting ----------------------------------------------------------

    fn el(
        role: &str,
        name: &str,
        node: u64,
        at: Option<(f64, f64, f64, f64)>,
    ) -> mcp_a11y::ElementInfo {
        mcp_a11y::ElementInfo {
            role: role.into(),
            name: Some(name.into()),
            value_preview: None,
            secure: false,
            bounds: at.map(|(x, y, w, h)| Bounds { x, y, w, h }),
            node_id: Some(node),
            state: Default::default(),
            semantic_intent: None,
            bound_state: None,
        }
    }

    fn install(arena: &Arc<Mutex<SnapshotArena>>, items: Vec<(&str, mcp_a11y::ElementInfo)>) {
        arena.lock().unwrap().install(mcp_a11y::Snapshot {
            id: "s9".into(),
            app: Some("App".into()),
            window: None,
            elements: items.into_iter().map(|(r, i)| (r.to_string(), i)).collect(),
            skeleton: false,
        });
    }

    #[tokio::test]
    async fn a_ref_may_be_spelled_with_or_without_the_at_or_as_a_number() {
        let (module, backend, _) = test_module();
        for r in [json!("@e4"), json!("e4"), json!("4"), json!(4)] {
            let env = call(&module, "ui_action", json!({ "ref": r, "action": "click" })).await;
            assert!(env.ok, "{r}: {env:?}");
            assert_eq!(env.data.unwrap()["ref"], "@e4");
        }
        assert_eq!(backend.actions.lock().unwrap().len(), 4);
        let env = call(
            &module,
            "ui_action",
            json!({ "ref": "button", "action": "click" }),
        )
        .await;
        assert_eq!(env.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn a_name_resolves_and_the_result_says_which_ref_it_was() {
        let (module, backend, _) = test_module();
        let env = call(
            &module,
            "ui_action",
            json!({ "name": "save changes", "action": "click" }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        let data = env.data.unwrap();
        assert_eq!(data["ref"], "@e4");
        assert_eq!(data["matches"], 1);
        assert_eq!(
            backend.actions.lock().unwrap()[0],
            (104, SemanticAction::Click, None)
        );
    }

    #[tokio::test]
    async fn the_better_name_wins_and_ties_are_reported() {
        let (module, backend, arena) = test_module();
        install(
            &arena,
            vec![
                ("@e1", el("button", "Save As…", 1, None)),
                ("@e2", el("button", "Save", 2, None)),
                ("@e3", el("button", "Save", 3, None)),
                ("@e4", el("link", "Save", 4, None)),
            ],
        );
        let env = call(
            &module,
            "ui_action",
            json!({ "name": "Save", "action": "click" }),
        )
        .await;
        let data = env.data.unwrap();
        // Exact beats prefix; the exact ones tie, and document order breaks it.
        assert_eq!(data["ref"], "@e2");
        assert_eq!(data["matches"], 3, "e2, e3 and the link tie on name");
        assert_eq!(backend.actions.lock().unwrap()[0].0, 2);

        // The role narrows it, with synonyms.
        let env = call(
            &module,
            "ui_action",
            json!({ "name": "Save", "role": "hyperlink", "action": "click" }),
        )
        .await;
        assert_eq!(env.data.unwrap()["ref"], "@e4");
    }

    /// The old lookup walked a HashMap: which of two equal matches was acted on
    /// changed from run to run.
    #[tokio::test]
    async fn equal_matches_resolve_to_document_order_every_time() {
        for _ in 0..10 {
            let (module, _, arena) = test_module();
            let items: Vec<(String, mcp_a11y::ElementInfo)> = (1..=30)
                .map(|n| (format!("@e{n}"), el("button", "OK", n, None)))
                .collect();
            install(
                &arena,
                items.iter().map(|(r, i)| (r.as_str(), i.clone())).collect(),
            );
            let env = call(
                &module,
                "ui_action",
                json!({ "name": "ok", "action": "click" }),
            )
            .await;
            assert_eq!(env.data.unwrap()["ref"], "@e1");
        }
    }

    #[tokio::test]
    async fn a_name_with_no_match_is_not_found_and_offers_candidates() {
        let (module, backend, arena) = test_module();
        install(
            &arena,
            vec![
                ("@e1", el("button", "Save Draft", 1, None)),
                ("@e2", el("button", "Cancel", 2, None)),
                ("@e3", el("link", "Save and Close", 3, None)),
            ],
        );
        let env = call(
            &module,
            "ui_action",
            json!({ "name": "Save Document", "action": "click" }),
        )
        .await;
        assert!(!env.ok);
        let err = env.error.as_ref().unwrap();
        assert_eq!(err.code, ErrorCode::NotFound);
        let suggestion = err.suggestion.as_deref().unwrap();
        assert!(
            suggestion.contains("@e1 button \"Save Draft\""),
            "{suggestion}"
        );
        assert!(!suggestion.contains("Cancel"), "{suggestion}");
        let cands = env.data.unwrap()["candidates"].as_array().unwrap().clone();
        assert!(cands.len() <= 5 && cands.len() == 2);
        assert_eq!(cands[0]["ref"], "@e1");
        assert!(backend.actions.lock().unwrap().is_empty());
    }

    /// A miss in a nearly empty tree is where a custom-drawn app lands: the
    /// label is on screen, just not in the tree. The error says to read it off
    /// the screen, and nothing is clicked or captured on the model's behalf.
    #[tokio::test]
    async fn a_name_miss_in_a_sparse_tree_points_at_ocr() {
        let (module, backend, arena) = test_module();
        install(&arena, vec![("@e1", el("window", "Canvas", 1, None))]);
        let env = call(
            &module,
            "ui_action",
            json!({ "name": "Save", "action": "click" }),
        )
        .await;
        let err = env.error.as_ref().unwrap();
        assert_eq!(err.code, ErrorCode::NotFound);
        let hint = err.suggestion.as_deref().unwrap();
        assert!(hint.contains("ocr_region with find \"Save\""), "{hint}");
        assert!(hint.contains("mouse_action"), "{hint}");
        assert!(backend.actions.lock().unwrap().is_empty());

        // A populated tree is not blamed on the app.
        let items: Vec<(String, mcp_a11y::ElementInfo)> = (1..=12)
            .map(|i| (format!("@e{i}"), el("button", "Other", i, None)))
            .collect();
        install(
            &arena,
            items.iter().map(|(r, i)| (r.as_str(), i.clone())).collect(),
        );
        let env = call(
            &module,
            "ui_action",
            json!({ "name": "Save", "action": "click" }),
        )
        .await;
        let hint = env.error.as_ref().unwrap().suggestion.clone().unwrap();
        assert!(!hint.contains("ocr_region"), "{hint}");
    }

    /// An unobserved UI is observed on demand rather than refused, when the
    /// composition root wired a way to look.
    #[tokio::test]
    async fn naming_an_element_with_no_snapshot_takes_a_fresh_one() {
        use mcp_a11y::{RawSnapshot, UiNode};
        struct Fresh;
        #[async_trait]
        impl A11yBackend for Fresh {
            async fn snapshot(&self, _: &SnapshotRequest) -> Result<RawSnapshot, BackendError> {
                Ok(RawSnapshot {
                    root: UiNode {
                        role: "application".into(),
                        children: vec![UiNode {
                            role: "button".into(),
                            name: Some("Go".into()),
                            node_id: Some(77),
                            ..Default::default()
                        }],
                        ..Default::default()
                    },
                    app: Some("Fresh".into()),
                    window: None,
                    terminal_app: false,
                    partial: false,
                })
            }
            fn platform(&self) -> &'static str {
                "fake"
            }
        }
        let backend = Arc::new(MockBackend::default());
        let arena = Arc::new(Mutex::new(SnapshotArena::new()));
        let module = InputModule::new(backend.clone(), arena.clone(), InputPolicy::default())
            .with_observer(Arc::new(Fresh));
        let env = call(
            &module,
            "ui_action",
            json!({ "name": "go", "action": "click" }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        assert_eq!(env.data.unwrap()["ref"], "@e1");
        assert_eq!(backend.actions.lock().unwrap()[0].0, 77);
        assert!(arena.lock().unwrap().current().is_some());

        // Without an observer, the same call says what to do instead.
        let (bare, _, bare_arena) = test_module();
        *bare_arena.lock().unwrap() = SnapshotArena::new();
        let env = call(
            &bare,
            "ui_action",
            json!({ "name": "go", "action": "click" }),
        )
        .await;
        assert_eq!(env.error.unwrap().code, ErrorCode::StaleRef);
    }

    #[tokio::test]
    async fn fill_form_picks_the_same_field_every_time_and_lists_candidates() {
        let (module, backend, arena) = test_module();
        install(
            &arena,
            vec![
                ("@e1", el("textfield", "Name", 1, None)),
                ("@e2", el("textfield", "Name", 2, None)),
            ],
        );
        for _ in 0..5 {
            let env = call(
                &module,
                "ui_fill_form",
                json!({ "fields": [{ "name": "name", "value": "x" }] }),
            )
            .await;
            assert!(env.ok, "{env:?}");
            assert_eq!(env.data.unwrap()["results"][0]["matches"], 2);
        }
        assert!(backend.values.lock().unwrap().iter().all(|(n, _)| *n == 1));

        let env = call(
            &module,
            "ui_fill_form",
            json!({ "fields": [{ "name": "Nome", "value": "x" }, ] }),
        )
        .await;
        assert!(!env.ok);
        assert_eq!(env.error.unwrap().code, ErrorCode::NotFound);
    }

    // ---- element-relative pointer -------------------------------------------

    fn with_button(arena: &Arc<Mutex<SnapshotArena>>) {
        install(
            arena,
            vec![(
                "@e1",
                el("button", "Target", 1, Some((100.0, 200.0, 60.0, 20.0))),
            )],
        );
    }

    #[tokio::test]
    async fn a_pointer_action_aims_at_the_centre_or_an_offset_from_the_corner() {
        let (module, backend, arena) = test_module();
        with_button(&arena);
        let at = |env: Envelope| {
            let d = env.data.unwrap();
            (
                d["at"]["x"].as_f64().unwrap(),
                d["at"]["y"].as_f64().unwrap(),
            )
        };
        let env = call(
            &module,
            "mouse_action",
            json!({ "type": "click", "ref": "@e1" }),
        )
        .await;
        assert_eq!(at(env), (130.0, 210.0));
        let env = call(
            &module,
            "mouse_action",
            json!({ "type": "click", "name": "target", "x": 5, "y": "3" }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        assert_eq!(at(env), (105.0, 203.0));
        // Only one axis given: the other stays centred.
        let env = call(&module, "hover", json!({ "ref": 1, "x": 10 })).await;
        assert_eq!(at(env), (110.0, 210.0));
        let log = backend.pointer.lock().unwrap();
        assert_eq!(log[0], (MouseKind::Click, 130.0, 210.0));
        assert_eq!(log[1], (MouseKind::Click, 105.0, 203.0));
    }

    #[tokio::test]
    async fn absolute_coordinates_still_work_and_numeric_strings_count() {
        let (module, backend, arena) = test_module();
        with_button(&arena);
        let env = call(
            &module,
            "mouse_action",
            json!({ "type": "click", "x": "120", "y": "205.5" }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        assert_eq!(
            backend.pointer.lock().unwrap()[0],
            (MouseKind::Click, 120.0, 205.5)
        );
        // A non-number is an error, not a missing argument.
        let env = call(
            &module,
            "mouse_action",
            json!({ "type": "click", "x": "left", "y": 1 }),
        )
        .await;
        let err = env.error.unwrap();
        assert_eq!(err.code, ErrorCode::InvalidArgs);
        assert!(
            err.message.contains("'x' must be a number"),
            "{}",
            err.message
        );
        let env = call(&module, "mouse_action", json!({ "type": "click", "x": 5 })).await;
        assert_eq!(env.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    /// An element-relative point is still a point: it goes through the same
    /// clamp as an absolute one.
    #[tokio::test]
    async fn clamp_input_applies_to_element_relative_points() {
        let backend = Arc::new(MockBackend::default());
        let arena = Arc::new(Mutex::new(SnapshotArena::new()));
        with_button(&arena);
        let module = InputModule::new(backend.clone(), arena, InputPolicy::default());
        let env = call(
            &module,
            "mouse_action",
            json!({ "type": "click", "ref": "@e1", "x": 5000, "y": 5 }),
        )
        .await;
        assert_eq!(env.error.unwrap().code, ErrorCode::PolicyDenied);
        let env = call(&module, "hover", json!({ "ref": "@e1", "x": -50, "y": 5 })).await;
        assert_eq!(env.error.unwrap().code, ErrorCode::PolicyDenied);
        let env = call(
            &module,
            "drag_drop",
            json!({ "from": { "ref": "@e1" }, "to": { "ref": "@e1", "x": 9999 } }),
        )
        .await;
        assert_eq!(env.error.unwrap().code, ErrorCode::PolicyDenied);
        assert!(backend.pointer.lock().unwrap().is_empty());
        assert!(backend.drags.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn scroll_and_hover_accept_a_name_and_numeric_strings() {
        let (module, backend, arena) = test_module();
        with_button(&arena);
        let env = call(
            &module,
            "scroll",
            json!({ "name": "Target", "direction": "Page-Down", "amount": "4" }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        assert_eq!(backend.scrolls.lock().unwrap()[0], (130.0, 210.0, 4));
        assert_eq!(env.data.unwrap()["ref"], "@e1");
    }

    /// Cmd+wheel is zoom in most apps, and `scroll` had no way to ask for it.
    /// The names are the ones `mouse_action` and `drag_drop` take, reach the
    /// backend as given, and come back in the result.
    #[tokio::test]
    async fn scroll_passes_modifiers_to_the_backend_and_reports_them() {
        let (module, backend, _arena) = test_module();
        let env = call(
            &module,
            "scroll",
            json!({ "x": 10, "y": 20, "direction": "down", "modifiers": ["cmd"] }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        assert_eq!(
            backend.scroll_modifiers.lock().unwrap()[0],
            vec!["cmd".to_string()]
        );
        assert_eq!(env.data.unwrap()["modifiers"], json!(["cmd"]));

        // Without the field nothing is held, and the result says so.
        let env = call(
            &module,
            "scroll",
            json!({ "x": 10, "y": 20, "direction": "down" }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        assert!(backend.scroll_modifiers.lock().unwrap()[1].is_empty());
        assert_eq!(env.data.unwrap()["modifiers"], json!([]));

        // An unknown name is refused before the wheel turns.
        let env = call(
            &module,
            "scroll",
            json!({ "x": 10, "y": 20, "direction": "down", "modifiers": ["hyper"] }),
        )
        .await;
        assert!(!env.ok);
        assert_eq!(env.error.as_ref().unwrap().code, ErrorCode::InvalidArgs);
        assert_eq!(backend.scrolls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn drag_drop_takes_objects_flat_keys_and_strings() {
        let (module, backend, arena) = test_module();
        install(
            &arena,
            vec![
                (
                    "@e1",
                    el("listitem", "Row A", 1, Some((0.0, 0.0, 100.0, 20.0))),
                ),
                (
                    "@e2",
                    el("listitem", "Row B", 2, Some((0.0, 40.0, 100.0, 20.0))),
                ),
            ],
        );
        let shapes = [
            json!({ "from": { "ref": "@e1" }, "to": { "name": "Row B" } }),
            json!({ "from_ref": "@e1", "to_name": "row b" }),
            json!({ "from": "@e1", "to": "Row B" }),
            json!({ "from": { "x": 50, "y": 10 }, "to": { "x": "50", "y": "50" } }),
            json!({ "from_x": 50, "from_y": 10, "to_x": 50, "to_y": 50 }),
        ];
        for shape in shapes {
            let env = call(&module, "drag_drop", shape.clone()).await;
            assert!(env.ok, "{shape}: {env:?}");
        }
        {
            let drags = backend.drags.lock().unwrap();
            assert_eq!(drags.len(), 5);
            for d in drags.iter() {
                assert_eq!((d.0, d.1), ((50.0, 10.0), (50.0, 50.0)));
            }
        }

        // Offsets from the corner, steps and hold as numeric strings.
        let env = call(
            &module,
            "drag_drop",
            json!({
                "from": { "ref": "@e1", "x": 5, "y": 5 }, "to": { "ref": "@e2", "x": 5, "y": 5 },
                "steps": "7", "hold_ms": "250",
            }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        assert_eq!(
            *backend.drags.lock().unwrap().last().unwrap(),
            ((5.0, 5.0), (5.0, 45.0), 7, 250)
        );
        let d = env.data.unwrap();
        assert_eq!(d["from"]["ref"], "@e1");
        assert_eq!(d["hold_ms"], 250);

        // The hold is capped, and a missing end is named.
        let env = call(
            &module,
            "drag_drop",
            json!({ "from_x": 1, "from_y": 1, "to_x": 2, "to_y": 2, "hold_ms": 999999 }),
        )
        .await;
        assert_eq!(env.data.unwrap()["hold_ms"], MAX_HOLD_MS);
        let env = call(&module, "drag_drop", json!({ "from": { "x": 1, "y": 1 } })).await;
        let err = env.error.unwrap();
        assert_eq!(err.code, ErrorCode::InvalidArgs);
        assert!(err.message.contains("'to'"), "{}", err.message);
    }

    // ---- read-back ----------------------------------------------------------

    #[test]
    fn a_write_that_changed_nothing_and_is_not_what_was_asked_fails() {
        let e = judge_write(Some("old"), Some("old"), "new", false).unwrap_err();
        assert!(e.contains("kept \"old\""), "{e}");
        // The same, for a secret: the message never carries the value.
        let e = judge_write(Some("hunter2"), Some("hunter2"), "new", true).unwrap_err();
        assert!(!e.contains("hunter2"), "{e}");
    }

    #[test]
    fn a_write_that_landed_is_ok_even_if_nothing_needed_changing() {
        let d = judge_write(Some("same"), Some("same"), "same", false).unwrap();
        assert_eq!(d["changed"], false);
        assert_eq!(d["value_after"], "same");
        assert!(d.get("reformatted").is_none());
    }

    /// A number field turning 1000 into 1,000 is working, not rejecting.
    #[test]
    fn a_field_that_reformats_what_was_written_is_reported_not_failed() {
        let d = judge_write(Some("0"), Some("1,000"), "1000", false).unwrap();
        assert_eq!(d["changed"], true);
        assert_eq!(d["value_after"], "1,000");
        assert_eq!(d["reformatted"], true);
    }

    #[test]
    fn secrets_never_echo_the_value_and_unreadable_fields_are_unverified() {
        let d = judge_write(Some(""), Some("••••"), "hunter2", true).unwrap();
        assert!(d.get("value_after").is_none() && d.get("reformatted").is_none());
        assert_eq!(d["changed"], true);
        let d = judge_write(Some("a"), None, "b", false).unwrap();
        assert_eq!(d["verified"], false);
        assert!(d.get("value_after").is_none());
    }

    #[tokio::test]
    async fn set_value_fails_when_the_field_keeps_its_old_value() {
        let (module, backend, _) = test_module();
        backend.script(101, vec![shown("old")]);
        let env = call(&module, "set_value", json!({ "ref": "@e1", "text": "new" })).await;
        assert!(!env.ok);
        let err = env.error.unwrap();
        assert_eq!(err.code, ErrorCode::ActionFailed);
        assert!(err.message.contains("kept \"old\""), "{}", err.message);
    }

    #[tokio::test]
    async fn set_value_reports_the_value_after_and_hides_a_secret() {
        let (module, backend, _) = test_module();
        backend.script(101, vec![shown("old"), shown("old"), shown("1,000")]);
        let env = call(
            &module,
            "set_value",
            json!({ "ref": "@e1", "text": "1000" }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        let d = env.data.unwrap();
        assert_eq!(d["value_after"], "1,000");
        assert_eq!(d["changed"], true);
        assert_eq!(d["ref"], "@e1");

        backend.script(102, vec![shown(""), shown("••••••")]);
        let env = call(
            &module,
            "set_value",
            json!({ "ref": "@e2", "text": "hunter2", "secret": true }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        let text = serde_json::to_string(&env).unwrap();
        assert!(!text.contains("hunter2") && !text.contains("••"), "{text}");
    }

    #[tokio::test]
    async fn keyboard_type_with_a_target_reads_the_field_back() {
        let (module, backend, _) = test_module();
        backend.script(101, vec![shown("ab"), shown("abcd")]);
        let env = call(
            &module,
            "keyboard_type",
            json!({ "ref": "@e1", "text": "cd" }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        let d = env.data.unwrap();
        assert_eq!(d["value_after"], "abcd");
        assert_eq!(d["changed"], true);
        assert_eq!(d["typed"], 2);

        // A text field that shows exactly what it did before did not take it.
        backend.script(101, vec![shown("full")]);
        let env = call(
            &module,
            "keyboard_type",
            json!({ "ref": "@e1", "text": "x" }),
        )
        .await;
        let err = env.error.unwrap();
        assert_eq!(err.code, ErrorCode::ActionFailed);
        assert!(err.message.contains("did not change"), "{}", err.message);

        // No target, nothing to read back.
        let env = call(&module, "keyboard_type", json!({ "text": "x" })).await;
        assert!(env.ok);
        assert!(env.data.unwrap().get("value_after").is_none());
    }

    #[tokio::test]
    async fn check_is_idempotent_and_verified() {
        let (module, backend, _) = test_module();
        // Already checked: asking for checked presses nothing.
        backend.script(103, vec![checked(true)]);
        let env = call(
            &module,
            "ui_action",
            json!({ "ref": "@e3", "action": "check" }),
        )
        .await;
        let d = env.data.unwrap();
        assert_eq!(
            (d["checked"].clone(), d["changed"].clone()),
            (json!(true), json!(false))
        );
        assert!(backend.actions.lock().unwrap().is_empty());

        // Unchecked, and the press takes.
        backend.script(103, vec![checked(false), checked(true)]);
        *backend.reads.lock().unwrap() = Default::default();
        let env = call(
            &module,
            "ui_action",
            json!({ "ref": "@e3", "action": "check" }),
        )
        .await;
        let d = env.data.unwrap();
        assert_eq!(
            (d["checked"].clone(), d["changed"].clone()),
            (json!(true), json!(true))
        );
        assert_eq!(backend.actions.lock().unwrap().len(), 1);

        // Unchecked, and the press does nothing: an error, not an ok.
        backend.script(103, vec![checked(false)]);
        *backend.reads.lock().unwrap() = Default::default();
        let env = call(
            &module,
            "ui_action",
            json!({ "ref": "@e3", "action": "check" }),
        )
        .await;
        assert!(!env.ok);
        assert!(env.error.unwrap().message.contains("still unchecked"));

        // Toggle flips whatever it read.
        backend.script(103, vec![checked(true), checked(false)]);
        *backend.reads.lock().unwrap() = Default::default();
        let env = call(
            &module,
            "ui_action",
            json!({ "ref": "@e3", "action": "toggle" }),
        )
        .await;
        assert_eq!(env.data.unwrap()["checked"], false);
    }

    #[tokio::test]
    async fn a_backend_that_cannot_read_gets_the_plain_press() {
        let (module, backend, _) = test_module();
        let env = call(
            &module,
            "ui_action",
            json!({ "ref": "@e3", "action": "uncheck" }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        assert_eq!(
            backend.actions.lock().unwrap()[0].1,
            SemanticAction::Uncheck
        );
    }

    // ---- choosing options ---------------------------------------------------

    #[tokio::test]
    async fn select_with_an_option_reports_what_the_control_shows() {
        let (module, backend, _) = test_module();
        let env = call(
            &module,
            "ui_action",
            json!({ "ref": "@e5", "action": "select", "option": "Dark" }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        let d = env.data.unwrap();
        assert_eq!(d["selected"], "Dark");
        assert_eq!(d["changed"], true);
        assert_eq!(
            backend.actions.lock().unwrap()[0],
            (105, SemanticAction::Select, Some("Dark".into()))
        );
    }

    #[tokio::test]
    async fn a_failed_choice_surfaces_the_backends_reason() {
        let (module, backend, _) = test_module();
        *backend.choice.lock().unwrap() = Some(Err(InputError::InvalidArgs(
            "no option \"Blue\"; the options are: \"Light\", \"Dark\"".into(),
        )));
        let env = call(
            &module,
            "ui_action",
            json!({ "ref": "@e5", "action": "select", "option": "Blue" }),
        )
        .await;
        let err = env.error.unwrap();
        assert_eq!(err.code, ErrorCode::InvalidArgs);
        assert!(err.message.contains("\"Light\", \"Dark\""));
    }

    #[tokio::test]
    async fn select_without_an_option_still_just_presses() {
        let (module, backend, _) = test_module();
        let env = call(
            &module,
            "ui_action",
            json!({ "ref": "@e5", "action": "select" }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        assert_eq!(
            backend.actions.lock().unwrap()[0],
            (105, SemanticAction::Select, None)
        );
        assert!(env.data.unwrap().get("selected").is_none());
    }

    /// Pointing at a menu item chooses it through the popup it belongs to, the
    /// way an HTML option resolves to its select.
    #[tokio::test]
    async fn selecting_a_menu_item_chooses_it_by_its_own_title() {
        let (module, backend, arena) = test_module();
        install(&arena, vec![("@e1", el("menuitem", "Rich Text", 9, None))]);
        let env = call(
            &module,
            "ui_action",
            json!({ "ref": "@e1", "action": "select" }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        assert_eq!(env.data.unwrap()["selected"], "Rich Text");
        assert_eq!(
            backend.actions.lock().unwrap()[0],
            (9, SemanticAction::Select, Some("Rich Text".into()))
        );

        // A menu-bar item has no popup: it falls back to the press it always was.
        *backend.choice.lock().unwrap() = Some(Err(InputError::Unsupported("no popup".into())));
        backend.actions.lock().unwrap().clear();
        let env = call(
            &module,
            "ui_action",
            json!({ "ref": "@e1", "action": "select" }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        let acts = backend.actions.lock().unwrap();
        assert_eq!(acts.last().unwrap(), &(9, SemanticAction::Select, None));
    }

    // ---- modifiers and schemas ----------------------------------------------

    #[test]
    fn pointer_modifiers_are_case_insensitive() {
        let m = parse_modifiers("t", &json!({ "modifiers": ["Cmd", " SHIFT "] })).unwrap();
        assert_eq!(m, vec!["cmd".to_string(), "shift".to_string()]);
    }

    #[test]
    fn schemas_stay_in_the_gemini_safe_subset() {
        /// Schema keywords only: a property may be *named* `format`.
        fn banned(v: &Value, path: &str, hits: &mut Vec<String>) {
            match v {
                Value::Object(m) => {
                    for (k, child) in m {
                        if path != "properties"
                            && matches!(k.as_str(), "pattern" | "format" | "additionalProperties")
                        {
                            hits.push(k.clone());
                        }
                        banned(child, k, hits);
                    }
                }
                Value::Array(a) => a.iter().for_each(|c| banned(c, "", hits)),
                _ => {}
            }
        }
        let (module, _, _) = test_module();
        for d in module.descriptors() {
            let mut hits = Vec::new();
            banned(&d.input_schema, "", &mut hits);
            assert!(hits.is_empty(), "{} uses {hits:?}", d.name);
        }
    }

    #[test]
    fn every_targeting_tool_declares_ref_name_and_role() {
        let (module, _, _) = test_module();
        let all = module.descriptors();
        for tool in [
            "ui_action",
            "set_value",
            "keyboard_type",
            "mouse_action",
            "scroll",
            "hover",
        ] {
            let d = all.iter().find(|d| d.name == tool).unwrap();
            for p in ["ref", "name", "role"] {
                assert!(
                    d.input_schema["properties"].get(p).is_some(),
                    "{tool} lacks {p}"
                );
            }
        }
        let fill = all.iter().find(|d| d.name == "ui_fill_form").unwrap();
        let item = &fill.input_schema["properties"]["fields"]["items"]["properties"];
        for p in ["ref", "name", "role", "option", "value", "action"] {
            assert!(item.get(p).is_some(), "fill_form field lacks {p}");
        }
        // The drag ends are declared, not bare objects.
        let drag = all.iter().find(|d| d.name == "drag_drop").unwrap();
        let from = &drag.input_schema["properties"]["from"]["properties"];
        for p in ["ref", "name", "x", "y"] {
            assert!(from.get(p).is_some(), "drag from lacks {p}");
        }
        for p in [
            "from_ref", "to_ref", "from_x", "from_y", "to_x", "to_y", "hold_ms",
        ] {
            assert!(
                drag.input_schema["properties"].get(p).is_some(),
                "drag lacks {p}"
            );
        }
    }

    /// The list is re-sent to the model on every turn; long prose belongs in
    /// the docs-only details, which `tools/list` omits.
    #[test]
    fn the_wire_text_stays_lean_and_the_prose_is_documented() {
        let (module, _, _) = test_module();
        let all = module.descriptors();
        let wire = |d: &ToolDescriptor| {
            d.name.len() + d.description.len() + d.input_schema.to_string().len()
        };
        for d in &all {
            assert!(
                wire(d) < 3_000,
                "{} is {} chars on the wire",
                d.name,
                wire(d)
            );
            assert!(d.description.len() < 400, "{} description is long", d.name);
        }
        let act = all.iter().find(|d| d.name == "ui_action").unwrap();
        assert!(act
            .details
            .as_deref()
            .is_some_and(|t| t.contains("reads back")));
        assert!(!act.description.contains("reads back"));
        let key = all.iter().find(|d| d.name == "keyboard_shortcut").unwrap();
        assert!(key
            .details
            .as_deref()
            .is_some_and(|t| t.contains("cmdorctrl")));
    }
}
