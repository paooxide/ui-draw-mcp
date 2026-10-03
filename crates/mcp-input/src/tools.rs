use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mcp_a11y::{Bounds, RefError, SnapshotArena};
use mcp_types::{CallCtx, Category, Envelope, ErrorCode, Tier, ToolDescriptor, ToolModule};
use serde_json::{json, Value};

use crate::backend::{
    valid_combo, ClipFormat, InputBackend, InputError, MouseKind, ScrollDir, SemanticAction,
};

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
            glide: Mutex::new(crate::glide::GlideConfig::default()),
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

    /// Configure smooth cursor gliding for demos/recordings.
    pub fn with_glide(mut self, g: crate::glide::GlideConfig) -> Self {
        self.glide = Mutex::new(g);
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

    /// Resolve a target spec (`{ref}` or `{name}`) to its backend node id, bounds, and identifier label.
    #[allow(clippy::result_large_err)]
    fn resolve_target(
        &self,
        tool: &str,
        spec: &Value,
    ) -> Result<(u64, Option<Bounds>, String), Envelope> {
        if let Some(reff) = spec.get("ref").and_then(Value::as_str) {
            let (id, bounds) = self.resolve_ref(tool, reff)?;
            Ok((id, bounds, reff.to_string()))
        } else if let Some(target_name) = spec.get("name").and_then(Value::as_str) {
            let arena = self.arena.lock().unwrap_or_else(|e| e.into_inner());
            let snap = arena.current().ok_or_else(|| {
                Envelope::fail_with(
                    tool,
                    ErrorCode::StaleRef,
                    "no current snapshot to resolve elements by name",
                    "call get_ui_tree first",
                )
            })?;
            let lower = target_name.to_lowercase();
            let mut found = None;
            for (reff, info) in &snap.elements {
                if let Some(n) = &info.name {
                    let n_lower = n.to_lowercase();
                    if n_lower == lower || n_lower.contains(&lower) {
                        if let Some(id) = info.node_id {
                            found = Some((id, info.bounds, reff.clone()));
                            break;
                        }
                    }
                }
            }
            found.ok_or_else(|| {
                Envelope::fail(
                    tool,
                    ErrorCode::NotFound,
                    format!("no element matching name '{target_name}' in current snapshot"),
                )
            })
        } else {
            Err(Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                "target must specify 'ref' or 'name'",
            ))
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
        let secret = args.get("secret").and_then(Value::as_bool).unwrap_or(false);
        if let Some(deny) = self.destructive_check(tool, text, secret).await {
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

        let mut results = Vec::with_capacity(fields_arr.len());
        let mut filled_count = 0;

        for field in fields_arr {
            let (node_id, _bounds, target_id) = match self.resolve_target(tool, field) {
                Ok(t) => t,
                Err(e) => {
                    return Envelope {
                        ok: false,
                        tool: tool.into(),
                        data: Some(json!({
                            "filled": filled_count,
                            "results": results,
                            "error_target": field.get("ref").or_else(|| field.get("name")),
                        })),
                        error: e.error,
                        image: None,
                    };
                }
            };

            let secret = field
                .get("secret")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let action_str = field.get("action").and_then(Value::as_str);

            if let Some(val) = field.get("value") {
                if let Some(b) = val.as_bool() {
                    let sem_act = match action_str {
                        Some("toggle") => SemanticAction::Toggle,
                        Some("click") => SemanticAction::Click,
                        _ => {
                            if b {
                                SemanticAction::Check
                            } else {
                                SemanticAction::Uncheck
                            }
                        }
                    };
                    if let Err(e) = self.backend.perform(node_id, sem_act, None).await {
                        return input_err(tool, e);
                    }
                    results.push(json!({
                        "target": target_id,
                        "action": format!("{sem_act:?}").to_lowercase(),
                        "value": b,
                        "status": "ok"
                    }));
                } else {
                    let text = if let Some(s) = val.as_str() {
                        s.to_string()
                    } else {
                        val.to_string()
                    };

                    if let Some(act) = action_str {
                        if act == "select" {
                            let opt = field.get("option").and_then(Value::as_str).unwrap_or(&text);
                            if let Err(e) = self
                                .backend
                                .perform(node_id, SemanticAction::Select, Some(opt))
                                .await
                            {
                                return input_err(tool, e);
                            }
                            results.push(json!({
                                "target": target_id,
                                "action": "select",
                                "option": opt,
                                "status": "ok"
                            }));
                        } else if let Some(sem_act) = SemanticAction::parse(act) {
                            if let Err(e) = self.backend.perform(node_id, sem_act, None).await {
                                return input_err(tool, e);
                            }
                            results.push(json!({
                                "target": target_id,
                                "action": act,
                                "status": "ok"
                            }));
                        } else {
                            if let Some(deny) = self.destructive_check(tool, &text, secret).await {
                                return deny;
                            }
                            if let Err(e) = self.backend.set_value(node_id, &text).await {
                                return input_err(tool, e);
                            }
                            let display_val = if secret {
                                format!("[REDACTED:len={}]", text.len())
                            } else {
                                text
                            };
                            results.push(json!({
                                "target": target_id,
                                "action": "set_value",
                                "value": display_val,
                                "status": "ok"
                            }));
                        }
                    } else {
                        if let Some(deny) = self.destructive_check(tool, &text, secret).await {
                            return deny;
                        }
                        if let Err(e) = self.backend.set_value(node_id, &text).await {
                            return input_err(tool, e);
                        }
                        let display_val = if secret {
                            format!("[REDACTED:len={}]", text.len())
                        } else {
                            text
                        };
                        results.push(json!({
                            "target": target_id,
                            "action": "set_value",
                            "value": display_val,
                            "status": "ok"
                        }));
                    }
                }
            } else if let Some(act) = action_str {
                let sem_act = SemanticAction::parse(act).unwrap_or(SemanticAction::Click);
                let opt = field.get("option").and_then(Value::as_str);
                if let Err(e) = self.backend.perform(node_id, sem_act, opt).await {
                    return input_err(tool, e);
                }
                results.push(json!({
                    "target": target_id,
                    "action": act,
                    "status": "ok"
                }));
            } else {
                return Envelope::fail(
                    tool,
                    ErrorCode::InvalidArgs,
                    "field must provide 'value' or 'action'",
                );
            }

            filled_count += 1;
        }

        let submitted = if let Some(submit_spec) = args.get("submit") {
            let (sub_id, _, _) = match self.resolve_target(tool, submit_spec) {
                Ok(t) => t,
                Err(e) => return e,
            };
            let act = submit_spec
                .get("action")
                .and_then(Value::as_str)
                .and_then(SemanticAction::parse)
                .unwrap_or(SemanticAction::Click);
            if let Err(e) = self.backend.perform(sub_id, act, None).await {
                return input_err(tool, e);
            }
            true
        } else {
            false
        };

        Envelope::ok(
            tool,
            json!({
                "filled": filled_count,
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
        let duration_ms = match args.get("duration_ms").and_then(Value::as_u64) {
            Some(ms) => Some(
                crate::glide::check_duration_ms(ms)
                    .map_err(|m| Box::new(Envelope::fail(tool, ErrorCode::InvalidArgs, m)))?,
            ),
            None => base.duration_ms,
        };
        let steps = args
            .get("steps")
            .and_then(Value::as_u64)
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
            Ok(()) => Envelope::ok(
                tool,
                json!({
                    "ok": true,
                    "modifiers": modifiers,
                    "glided": waypoints > 0
                }),
            ),
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

    async fn hover(&self, args: &Value, cancel: &mcp_types::CancelToken, since: u64) -> Envelope {
        let tool = "hover";
        let (x, y) = match self.parse_point(tool, args) {
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
            Ok(()) => Envelope::ok(tool, json!({ "ok": true, "glided": waypoints > 0 })),
            Err(e) => input_err(tool, e),
        }
    }

    async fn drag_drop(&self, args: &Value, since: u64) -> Envelope {
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
        match self.backend.drag(from, to, &modifiers, steps, since).await {
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
        if let Some(dur) = args.get("duration_ms").and_then(Value::as_u64) {
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
                "Set the value of a text element by ref (accessibility SetValue). This works                  on a background window and needs no focus. Set 'secret' for a password.",
                json!({"type":"object","properties":{
                    "ref":{"type":"string"},"text":{"type":"string"},
                    "secret":{"type":"boolean","description":"the text is a password or other secret: keep it out of the audit log and never send it to the judge"},
                    "expect": expect_schema()},"required":["ref","text"]}),
            ).idempotent(true),
            ToolDescriptor::new(
                "ui_fill_form",
                Category::Input,
                Tier::Standard,
                "Fill multiple native UI fields (text fields, checkboxes, switches, popups, radios) \
                 in one call, and optionally submit and verify postconditions. \
                 Target fields by element ref (@eN) or name/label.",
                json!({
                    "type": "object",
                    "properties": {
                        "fields": {
                            "type": "array",
                            "description": "list of fields to set or toggle",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "ref": { "type": "string", "description": "element ref @eN" },
                                    "name": { "type": "string", "description": "element name or label to match if ref is omitted" },
                                    "value": { "description": "string text, boolean for checkbox/switch, or selection option" },
                                    "action": {
                                        "type": "string",
                                        "enum": ["set_value", "click", "toggle", "check", "uncheck", "select", "focus"],
                                        "description": "action to perform (defaults to set_value for text, check/uncheck for boolean)"
                                    },
                                    "secret": { "type": "boolean", "description": "password or secret: redacts value from audit logs and output" }
                                }
                            }
                        },
                        "submit": {
                            "type": "object",
                            "description": "optional submission button to click after filling fields",
                            "properties": {
                                "ref": { "type": "string", "description": "submit button ref @eN" },
                                "name": { "type": "string", "description": "submit button name to match if ref is omitted" },
                                "action": { "type": "string", "enum": ["click", "toggle"] }
                            }
                        },
                        "expect": expect_schema()
                    },
                    "required": ["fields"]
                }),
            ),
            ToolDescriptor::new(
                "keyboard_type",
                Category::Input,
                Tier::Standard,
                "Type Unicode text into the focused window. If 'ref' is given, focus it                  first. Does not press return. Set 'secret' when typing a password so it is                  kept out of the audit log and never sent to the judge.",
                json!({"type":"object","properties":{
                    "text":{"type":"string"},"ref":{"type":"string","description":"optional element to focus first"},
                    "secret":{"type":"boolean","description":"the text is a password or other secret: keep it out of the audit log and never send it to the judge"},
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
                 selection. Set 'glide: true' or 'speed' to smoothly glide the pointer along a Bezier curve for demos.",
                json!({"type":"object","properties":{
                    "type":{"type":"string","enum":["move","click","double","triple","right_click","down","up"]},
                    "x":{"type":"number"},"y":{"type":"number"},
                    "button":{"type":"string","description":"left|right|middle"},
                    "modifiers":{"type":"array","items":{"type":"string","enum":["cmd","shift","opt","alt","ctrl","fn"]}},
                    "glide":{"type":"boolean","description":"smooth Bezier gliding to target coordinates"},
                    "speed":{"type":"string","enum":["cinematic","demo","snappy","instant"],"description":"gliding speed preset"},
                    "duration_ms":{"type":"integer","description":"custom gliding duration in milliseconds, 0-2000"},
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
                "Move the pointer over an element ref or point (reveals tooltips/hover menus). Supports smooth gliding via 'glide' or 'speed'.",
                json!({"type":"object","properties":{
                    "ref":{"type":"string"},"x":{"type":"number"},"y":{"type":"number"},
                    "glide":{"type":"boolean","description":"smooth Bezier gliding to target coordinates"},
                    "speed":{"type":"string","enum":["cinematic","demo","snappy","instant"],"description":"gliding speed preset"},
                    "duration_ms":{"type":"integer","description":"custom gliding duration in milliseconds, 0-2000"}},"required":[]}),
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
        InputError::InvalidArgs(m) => (ErrorCode::InvalidArgs, m),
        InputError::Failed(m) => (ErrorCode::ActionFailed, m),
    };
    Envelope::fail(tool, code, msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClipData;

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
    }

    #[async_trait]
    impl InputBackend for MockBackend {
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
        async fn type_text(&self, _text: &str) -> Result<(), InputError> {
            Ok(())
        }
        async fn key_combo(&self, _combo: &str) -> Result<(), InputError> {
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
            if matches!(kind, MouseKind::Move) {
                self.moves.lock().unwrap().push((x, y));
            }
            Ok(())
        }
        async fn scroll_at(
            &self,
            _x: f64,
            _y: f64,
            _dir: ScrollDir,
            _amount: i32,
        ) -> Result<(), InputError> {
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
            _from: (f64, f64),
            _to: (f64, f64),
            _modifiers: &[String],
            _steps: u32,
            _since_takeover: u64,
        ) -> Result<(), InputError> {
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
}
