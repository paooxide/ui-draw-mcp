use std::sync::Arc;

use async_trait::async_trait;
use mcp_a11y::A11yBackend;
use mcp_types::{CallCtx, Category, Envelope, ErrorCode, Tier, ToolDescriptor, ToolModule};
use serde_json::{json, Value};

use crate::backend::{DialogScope, Rect, TargetInfo, WindowAction, WindowBackend, WindowError};
use crate::wait::{default_wait_timeout, parse_wait_spec, wait_schema, WaitEvaluator};

/// The `window` engine: window/app/menu control and the `wait_for` settle
/// primitive. `wait_for` delegates to the shared [`WaitEvaluator`], which also
/// backs the `expect` clause on every input tool.
pub struct WindowModule {
    backend: Arc<dyn WindowBackend>,
    evaluator: WaitEvaluator,
    allowed_apps: Vec<String>,
    judge: Option<Arc<mcp_judge::Judge>>,
    foreground: mcp_types::Foreground,
}

impl WindowModule {
    pub fn new(
        backend: Arc<dyn WindowBackend>,
        a11y: Arc<dyn A11yBackend>,
        allowed_apps: Vec<String>,
    ) -> Self {
        WindowModule {
            evaluator: WaitEvaluator::new(backend.clone(), a11y),
            backend,
            allowed_apps,
            judge: None,
            foreground: mcp_types::Foreground::Allow,
        }
    }

    /// Apply the operator's `foreground` setting (composition root only).
    /// With `never`, calls that would bring something forward are refused
    /// before they reach the backend, and `launch` asks not to activate.
    pub fn with_foreground(mut self, foreground: mcp_types::Foreground) -> Self {
        self.foreground = foreground;
        self
    }

    /// Give `wait_for`'s evaluator the judge that answers a `judge` condition,
    /// and `handle_dialogs` the judge that ranks buttons against an intent.
    pub fn with_judge(mut self, judge: Arc<mcp_judge::Judge>) -> Self {
        self.evaluator = self.evaluator.with_judge(judge.clone());
        self.judge = Some(judge);
        self
    }

    fn app_allowed(&self, app: &str) -> bool {
        mcp_types::apps::app_allowed(&self.allowed_apps, app)
    }

    /// Resolve the application an `app` argument names, for a tool that acts
    /// on or reads it, and hold it to `allowed_apps`.
    ///
    /// The name is checked as given and again as the application it resolves
    /// to, so a loose spelling cannot reach an application the list excludes.
    /// A name that matches nothing is `NOT_FOUND` here, before the tool runs,
    /// so none of them can fall back to another application. `None` when no
    /// name was given.
    // Envelope is intentionally large (it can carry an image); it is the Err
    // type here only as a control-flow shortcut for an early return.
    #[allow(clippy::result_large_err)]
    async fn named_target(
        &self,
        tool: &str,
        app: Option<&str>,
    ) -> Result<Option<TargetInfo>, Envelope> {
        let Some(name) = app.map(str::trim).filter(|a| !a.is_empty()) else {
            return Ok(None);
        };
        if !self.app_allowed(name) {
            return Err(denied_app(tool, name));
        }
        let info = self
            .backend
            .describe_target(Some(name))
            .await
            .map_err(|e| win_err(tool, e))?;
        if !self.app_allowed(&info.app) {
            return Err(denied_app(tool, &info.app));
        }
        Ok(Some(info))
    }

    async fn list_windows(&self, args: &Value) -> Envelope {
        let tool = "list_windows";
        let app = str_arg(args, "app");
        let named = match self.named_target(tool, app.as_deref()).await {
            Ok(n) => n,
            Err(e) => return e,
        };
        // Naming nothing while nothing is pinned lists every application; the
        // result says so, so an empty list is not read as one app's.
        let target = match named {
            Some(t) => Some(t),
            None => self.backend.pinned().await,
        };
        match self.backend.list_windows(app.as_deref()).await {
            Ok(ws) => {
                let mut data = json!({ "windows": ws });
                match target {
                    Some(t) => data["target"] = json!(t),
                    None => data["scope"] = json!("all_apps"),
                }
                Envelope::ok(tool, data)
            }
            Err(e) => win_err(tool, e),
        }
    }

    async fn list_apps(&self) -> Envelope {
        match self.backend.list_apps().await {
            Ok(apps) => Envelope::ok("list_apps", json!({ "apps": apps })),
            Err(e) => win_err("list_apps", e),
        }
    }

    async fn launch(&self, args: &Value) -> Envelope {
        let Some(app) = str_arg(args, "app") else {
            return Envelope::fail("launch", ErrorCode::InvalidArgs, "missing 'app'");
        };
        if !self.app_allowed(&app) {
            return Envelope::fail(
                "launch",
                ErrorCode::PolicyDenied,
                format!("app '{app}' is not in policy.allowed_apps"),
            );
        }
        let started = if self.foreground.is_never() {
            self.backend.launch_in_background(&app).await
        } else {
            self.backend.launch(&app).await
        };
        match started {
            Ok(()) => Envelope::ok("launch", json!({ "ok": true })),
            Err(e) => win_err("launch", e),
        }
    }

    async fn close_app(&self, args: &Value) -> Envelope {
        let Some(target) = str_arg(args, "app").or_else(|| str_arg(args, "pid")) else {
            return Envelope::fail("close_app", ErrorCode::InvalidArgs, "missing 'app'");
        };
        match self.backend.close_app(&target).await {
            Ok(()) => Envelope::ok("close_app", json!({ "ok": true })),
            Err(e) => win_err("close_app", e),
        }
    }

    async fn control_window(&self, args: &Value) -> Envelope {
        let Some(action) = str_arg(args, "action").and_then(|s| WindowAction::parse(&s)) else {
            return Envelope::fail(
                "control_window",
                ErrorCode::InvalidArgs,
                "missing/invalid 'action'",
            );
        };
        let bounds = args.get("bounds").and_then(parse_rect);
        if matches!(action, WindowAction::Move | WindowAction::Resize) && bounds.is_none() {
            return Envelope::fail(
                "control_window",
                ErrorCode::InvalidArgs,
                "move/resize need 'bounds'",
            );
        }
        let app = str_arg(args, "app");
        let named = match self.named_target("control_window", app.as_deref()).await {
            Ok(n) => n,
            Err(e) => return e,
        };
        let target = match self
            .resolved_target("control_window", named, app.as_deref())
            .await
        {
            Ok(t) => t,
            Err(e) => return e,
        };
        match self
            .backend
            .control_window(
                app.as_deref(),
                str_arg(args, "title").as_deref(),
                action,
                bounds,
            )
            .await
        {
            Ok(()) => Envelope::ok("control_window", json!({ "ok": true, "target": target })),
            Err(e) => win_err("control_window", e),
        }
    }

    /// The application a call acts on: the one its `app` named, else the
    /// pinned one, else the frontmost, with which of the three it was.
    // Envelope is intentionally large; see `named_target`.
    #[allow(clippy::result_large_err)]
    async fn resolved_target(
        &self,
        tool: &str,
        named: Option<TargetInfo>,
        app: Option<&str>,
    ) -> Result<TargetInfo, Envelope> {
        match named {
            Some(t) => Ok(t),
            None => self
                .backend
                .describe_target(app)
                .await
                .map_err(|e| win_err(tool, e)),
        }
    }

    async fn menu(&self, tool: &str, args: &Value, invoke: bool) -> Envelope {
        let Some(path) = args.get("path").and_then(Value::as_array) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'path' array");
        };
        let path: Vec<String> = path
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        if path.is_empty() {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "'path' must be non-empty");
        }
        let app = str_arg(args, "app");
        let named = match self.named_target(tool, app.as_deref()).await {
            Ok(n) => n,
            Err(e) => return e,
        };
        let target = match self.resolved_target(tool, named, app.as_deref()).await {
            Ok(t) => t,
            Err(e) => return e,
        };
        let res = if invoke {
            self.backend.menu_invoke(app.as_deref(), &path).await
        } else {
            self.backend.menu_open(app.as_deref(), &path).await
        };
        match res {
            Ok(()) => Envelope::ok(tool, json!({ "ok": true, "target": target })),
            Err(e) => win_err(tool, e),
        }
    }

    async fn handle_dialogs(&self, args: &Value) -> Envelope {
        let tool = "handle_dialogs";
        let scope = match str_arg(args, "scope") {
            None => DialogScope::App,
            Some(s) => match DialogScope::parse(&s) {
                Some(v) => v,
                None => {
                    return Envelope::fail_with(
                        tool,
                        ErrorCode::InvalidArgs,
                        format!("unknown scope '{s}'"),
                        "use 'app' (this application) or 'system' (every on-screen app)",
                    )
                }
            },
        };
        let intent = str_arg(args, "intent");
        let app = str_arg(args, "app");
        // A system sweep reads every application and names none; only a scope
        // of one application has a target to resolve and an allowlist to meet.
        let target = if scope == DialogScope::App {
            let named = match self.named_target(tool, app.as_deref()).await {
                Ok(n) => n,
                Err(e) => return e,
            };
            match self.resolved_target(tool, named, app.as_deref()).await {
                Ok(t) => Some(t),
                Err(e) => return e,
            }
        } else {
            None
        };
        let ds = match self.backend.list_dialogs(app.as_deref(), scope).await {
            Ok(ds) => ds,
            Err(e) => return win_err(tool, e),
        };
        // A password prompt is a hand-back-to-the-human signal, not something
        // to fill in: surface it at the top level so it cannot be missed.
        let credential = ds.iter().any(|d| d.has_secure_field);
        let mut data = json!({
            "dialogs": ds,
            "count": ds.len(),
            "credential_prompt": credential,
        });
        if let Some(t) = target {
            data["target"] = json!(t);
        }
        // When the caller says what it is trying to do, ask the judge which
        // button serves that intent. This only advises: it does not press
        // anything, and the agent still acts through `ui_action` on a ref from
        // `get_ui_tree`. Modelled on `find_elements`' `describe`.
        if let Some(intent) = intent {
            match self.suggest_button(&intent, &ds).await {
                Ok(suggestion) => data["suggestion"] = suggestion,
                Err(e) => return e,
            }
        }
        Envelope::ok(tool, data)
    }

    /// Rank the buttons across the listed dialogs against `intent`. Returns the
    /// suggestion JSON, or a ready-to-return failure envelope.
    // Envelope is intentionally large (it carries an optional image); it is the
    // Err type here only as a control-flow shortcut for an early return.
    #[allow(clippy::result_large_err)]
    async fn suggest_button(
        &self,
        intent: &str,
        ds: &[crate::backend::DialogInfo],
    ) -> Result<Value, Envelope> {
        let tool = "handle_dialogs";
        let Some(judge) = self.judge.as_ref().filter(|j| j.enabled()) else {
            return Err(Envelope::fail_with(
                tool,
                ErrorCode::UnsupportedOs,
                "'intent' needs the judge, which is not enabled",
                "set [judge] enabled = \"true\" and provide TYPESAFE_API_KEY, or read the buttons and press one with ui_action yourself",
            ));
        };
        let candidates = button_candidates(ds);
        if candidates.is_empty() {
            // Nothing to choose between; not an error, just no suggestion.
            return Ok(json!({ "none": true, "reason": "the dialogs expose no buttons" }));
        }
        rank_dialog_buttons(judge, intent, ds, &candidates)
            .await
            .map_err(|e| {
                Envelope::fail_with(
                    tool,
                    ErrorCode::ActionFailed,
                    format!("could not rank the buttons: {}", e.message()),
                    "read the buttons and choose one yourself",
                )
            })
    }

    async fn menu_list(&self, args: &Value) -> Envelope {
        let tool = "menu_list";
        let path: Vec<String> = args
            .get("path")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let depth = args.get("depth").and_then(Value::as_u64).unwrap_or(1) as u32;
        let app = str_arg(args, "app");
        let named = match self.named_target(tool, app.as_deref()).await {
            Ok(n) => n,
            Err(e) => return e,
        };
        let target = match self.resolved_target(tool, named, app.as_deref()).await {
            Ok(t) => t,
            Err(e) => return e,
        };
        match self.backend.menu_list(app.as_deref(), &path, depth).await {
            Ok(items) => Envelope::ok(
                tool,
                json!({ "items": items, "count": items.len(), "target": target }),
            ),
            Err(e) => win_err(tool, e),
        }
    }

    /// Wait for the UI to settle.
    ///
    /// Synthetic input is asynchronous: the app processes it on its own run
    /// loop: so this is the primitive that makes observe-after-act reliable.
    /// The same evaluator backs every `expect` clause on an input tool, so the
    /// two cannot drift apart.
    async fn wait_for(&self, args: &Value, ctx: &CallCtx) -> Envelope {
        let mut spec = match parse_wait_spec(args, default_wait_timeout()) {
            Ok(s) => s,
            Err(msg) => return Envelope::fail("wait_for", ErrorCode::InvalidArgs, msg),
        };
        if let Err(e) = self.named_target("wait_for", spec.app.as_deref()).await {
            return e;
        }
        // Waiting on an application is not choosing to drive it: the polls
        // must not pin it or replace the elements the last get_ui_tree's refs
        // point at.
        spec.passive = true;
        let wants_judge = spec
            .conditions
            .iter()
            .any(|c| matches!(c, crate::wait::WaitCondition::Judged(_)));
        if wants_judge && !self.evaluator.judge_available() {
            return Envelope::fail_with(
                "wait_for",
                ErrorCode::UnsupportedOs,
                "'judge' needs the judge, which is not enabled",
                "set [judge] enabled = \"true\" and provide TYPESAFE_API_KEY, or wait on text, window, gone or focused",
            );
        }
        let outcome = self.evaluator.wait(&spec, ctx).await;
        let mut data = json!({ "ok": outcome.met, "waited_ms": outcome.waited_ms });
        if let Some(p) = outcome.judge_probability {
            data["judge_probability"] = json!((p * 1000.0).round() / 1000.0);
        }
        if outcome.met {
            Envelope::ok("wait_for", data)
        } else {
            let mut env = Envelope::fail_with(
                "wait_for",
                ErrorCode::Timeout,
                format!("condition not met after {}ms", outcome.waited_ms),
                "observe with get_ui_tree to see the current state",
            );
            if outcome.judge_probability.is_some() {
                env.data = Some(data);
            }
            env
        }
    }
}

impl WindowModule {
    /// Pin (or release) the session's application target.
    ///
    /// `activate: false` pins without bringing the application forward or
    /// touching its windows, which is the only form `foreground = "never"`
    /// lets through. Any other value of `activate` is the activating form:
    /// the tool layer has already refused it under `never`, and it is not
    /// guessed to mean the quiet one.
    async fn focus_app(&self, args: &Value) -> Envelope {
        let tool = "focus_app";
        let app = str_arg(args, "app");
        let quiet = match args.get("activate") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => !*b,
            Some(_) => {
                return Envelope::fail(tool, ErrorCode::InvalidArgs, "'activate' is true or false")
            }
        };
        let Some(name) = app.as_deref().map(str::trim).filter(|a| !a.is_empty()) else {
            return match self.backend.focus_app(None).await {
                Ok(_) => Envelope::ok(tool, json!({ "target": null, "released": true })),
                Err(e) => win_err(tool, e),
            };
        };
        // Resolved and held to the allowlist before anything is raised.
        if let Err(e) = self.named_target(tool, Some(name)).await {
            return e;
        }
        if quiet {
            return match self.backend.pin_app(name).await {
                Ok(t) => Envelope::ok(
                    tool,
                    json!({ "target": t.app, "pid": t.pid, "activated": false }),
                ),
                Err(e) => win_err(tool, e),
            };
        }
        match self.backend.focus_app(Some(name)).await {
            Ok(Some(target)) => Envelope::ok(tool, json!({ "target": target, "activated": true })),
            Ok(None) => Envelope::ok(tool, json!({ "target": null, "released": true })),
            Err(e) => win_err(tool, e),
        }
    }
}

#[async_trait]
impl ToolModule for WindowModule {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        vec![
            ToolDescriptor::new(
                "list_windows",
                Category::Window,
                Tier::Read,
                "List open windows: title, position, size and state (onscreen, minimized, \
                 or offscreen: another Space or a hidden app). With no 'app', lists every \
                 application's windows: start here to see what is open. With 'app', lists \
                 just that one, running or not frontmost. After focus_app has pinned an \
                 application, omitting 'app' means the pinned one. Desktop furniture (the \
                 Dock, menu bar, overlays) is left out. A window's id is its place in its \
                 own application's list: pass it with that app to capture_window or \
                 ocr_region. The result's target names the application a list is of.",
                json!({"type":"object","properties":{"app":{"type":"string",
                    "description":"application name, as list_apps reports it"}},"required":[]}),
            ).untrusted_output(),
            ToolDescriptor::new(
                "focus_app",
                Category::Window,
                Tier::Standard,
                "Pin this session to an application so every later get_ui_tree/action \
                 targets it instead of whatever happens to be frontmost. By default it \
                 also brings the app forward; activate=false pins it without raising it or \
                 touching its windows, to drive an app that stays in the background (the \
                 only form foreground=\"never\" allows). Omit 'app' to release the pin.",
                json!({"type":"object","properties":{
                    "app":{"type":"string","description":"application name, as list_apps reports it"},
                    "activate":{"type":"boolean","description":"false: pin without bringing the app forward (default true)"}
                },"required":[]}),
            )
            .idempotent(true),
            ToolDescriptor::new(
                "list_apps",
                Category::Window,
                Tier::Read,
                "List the applications that are running, by the name launch, focus_app, \
                 list_windows and get_ui_tree expect. Background daemons and the desktop's \
                 own interface are left out. Use list_windows to see what each one has open.",
                json!({"type":"object","properties":{},"required":[]}),
            ).untrusted_output(),
            ToolDescriptor::new(
                "launch",
                Category::Window,
                Tier::Standard,
                "Launch or bring forward an application by name (allowlist-gated).",
                json!({"type":"object","properties":{"app":{"type":"string"}},"required":["app"]}),
            )
            .idempotent(true),
            ToolDescriptor::new(
                "close_app",
                Category::Window,
                Tier::Standard,
                "Quit an application by name.",
                json!({"type":"object","properties":{"app":{"type":"string"}},"required":["app"]}),
            ),
            ToolDescriptor::new(
                "control_window",
                Category::Window,
                Tier::Standard,
                "Focus/move/resize/minimize/maximize/restore/close a window of 'app' (else \
                 the pinned app, else the frontmost) whose title contains 'title' (else its \
                 main window). The result's target says which app it acted on.",
                json!({"type":"object","properties":{
                    "app":{"type":"string"},"title":{"type":"string"},
                    "action":{"type":"string","enum":["focus","move","resize","minimize","maximize","restore","close"]},
                    "bounds":{"type":"object","properties":{"x":{"type":"number"},"y":{"type":"number"},"w":{"type":"number"},"h":{"type":"number"}}}
                },"required":["action"]}),
            ),
            ToolDescriptor::new(
                "menu_open",
                Category::Window,
                Tier::Standard,
                "Open a menu-bar menu by title path, e.g. [\"File\"].",
                json!({"type":"object","properties":{"app":{"type":"string"},"path":{"type":"array","items":{"type":"string"}}},"required":["path"]}),
            ),
            ToolDescriptor::new(
                "menu_invoke",
                Category::Window,
                Tier::Standard,
                "Open and click a menu item by title path, e.g. [\"File\",\"Save As…\"].",
                json!({"type":"object","properties":{"app":{"type":"string"},"path":{"type":"array","items":{"type":"string"}}},"required":["path"]}),
            ),
            ToolDescriptor::new(
                "handle_dialogs",
                Category::Window,
                Tier::Read,
                "List open dialogs, sheets, popovers and menus, with their buttons, message \
                 text and which button Return/Escape activates. scope='system' also finds \
                 prompts raised by another process, such as macOS authentication panels. \
                 Press a button with ui_action on the matching ref from get_ui_tree. Pass \
                 'intent' (what you are trying to do) to have the judge suggest which button \
                 serves it, in 'suggestion' (advice only; it presses nothing); needs the \
                 judge enabled).",
                json!({"type":"object","properties":{
                    "app":{"type":"string"},
                    "scope":{"type":"string","enum":["app","system"]},
                    "intent":{"type":"string","description":"what you are trying to accomplish; the judge suggests the button that serves it"}
                },"required":[]}),
            ).untrusted_output(),
            ToolDescriptor::new(
                "menu_list",
                Category::Window,
                Tier::Read,
                "Enumerate an app's menu bar: titles, enabled state, and the keyboard \
                 equivalent of each item. Use this to discover exact titles before \
                 menu_invoke, or to skip the menu entirely via keyboard_shortcut.",
                json!({"type":"object","properties":{
                    "app":{"type":"string"},
                    "path":{"type":"array","items":{"type":"string"},
                            "description":"menu to enumerate, e.g. [\"File\"]; omit for the menu bar"},
                    "depth":{"type":"integer","description":"levels to descend, 1-5 (default 1)"}
                },"required":[]}),
            ).untrusted_output(),
            ToolDescriptor::new(
                "wait_for",
                Category::Window,
                Tier::Read,
                "Block until the UI settles: text appears, a window appears, text is gone, \
                 or an element takes focus. Synthetic input is asynchronous, so observing \
                 straight after acting reads the previous state: wait first. Several \
                 conditions must all hold at once. 'app' watches that application without \
                 pinning it.",
                wait_schema("what to wait for; all given conditions must hold together"),
            ),
        ]
    }

    async fn call(&self, name: &str, args: Value, ctx: &CallCtx) -> Envelope {
        if self.foreground.is_never() {
            if let Some(refusal) = crate::foreground::tool_refusal(&*self.backend, name, &args) {
                return refusal.envelope(name);
            }
        }
        match name {
            "list_windows" => self.list_windows(&args).await,
            "list_apps" => self.list_apps().await,
            "launch" => self.launch(&args).await,
            "close_app" => self.close_app(&args).await,
            "control_window" => self.control_window(&args).await,
            "menu_open" => self.menu("menu_open", &args, false).await,
            "menu_invoke" => self.menu("menu_invoke", &args, true).await,
            "handle_dialogs" => self.handle_dialogs(&args).await,
            "menu_list" => self.menu_list(&args).await,
            "wait_for" => self.wait_for(&args, ctx).await,
            "focus_app" => self.focus_app(&args).await,
            other => Envelope::fail(other, ErrorCode::InvalidArgs, "unknown tool"),
        }
    }
}

fn denied_app(tool: &str, app: &str) -> Envelope {
    Envelope::fail(
        tool,
        ErrorCode::PolicyDenied,
        format!("app '{app}' is not in policy.allowed_apps"),
    )
}

fn str_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_string)
}

fn parse_rect(v: &Value) -> Option<Rect> {
    Some(Rect {
        x: v.get("x")?.as_f64()?,
        y: v.get("y")?.as_f64()?,
        w: v.get("w")?.as_f64()?,
        h: v.get("h")?.as_f64()?,
    })
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

/// Build the ranking candidates from every button on every listed dialog,
/// keyed `"<dialogId>:<label>"` so identical labels in two dialogs (two
/// "OK"s) stay distinct.
fn button_candidates(
    ds: &[crate::backend::DialogInfo],
) -> std::collections::BTreeMap<String, String> {
    let mut candidates = std::collections::BTreeMap::new();
    for d in ds {
        for b in &d.buttons {
            let where_ = d.title.as_deref().unwrap_or(&d.kind);
            candidates.insert(
                format!("{}:{}", d.id, b),
                format!("Button \"{b}\" in the dialog \"{where_}\""),
            );
        }
    }
    candidates
}

/// Split a `"<dialogId>:<label>"` candidate key back into its parts. A label
/// may itself contain a colon, so only the first is the separator; a key with
/// no colon (which the judge should never return) keeps the whole string as
/// the label.
fn parse_button_choice(choice: &str) -> (Option<u32>, String) {
    match choice.split_once(':') {
        Some((id, label)) => (id.parse().ok(), label.to_string()),
        None => (None, choice.to_string()),
    }
}

/// Ask the judge which button serves the intent, and shape the answer. The
/// caller has already checked the judge is enabled and that there is at least
/// one candidate.
async fn rank_dialog_buttons(
    judge: &mcp_judge::Judge,
    intent: &str,
    ds: &[crate::backend::DialogInfo],
    candidates: &std::collections::BTreeMap<String, String>,
) -> Result<Value, mcp_judge::JudgeError> {
    let state = json!({
        "intent": intent,
        "dialogs": ds,
        "candidates": candidates,
    });
    let r = judge
        .rank(
            state,
            "Which candidate button in `candidates` should be pressed to accomplish `intent`? Judge from each button's label and the text of the dialog it belongs to, shown in `dialogs`. Do not choose a destructive or irreversible button unless `intent` clearly asks for it.",
            candidates,
        )
        .await?;
    let (dialog, button) = parse_button_choice(&r.choice);
    Ok(json!({
        "button": button,
        "dialog": dialog,
        "confidence": r.confidence,
        "any_fits": r.any_fits,
        // A real match, not the least-bad of poor options.
        "confident": r.any_fits >= judge.match_threshold(),
    }))
}

#[cfg(test)]
mod dialog_suggestion_tests {
    use super::*;
    use crate::backend::DialogInfo;
    use async_trait::async_trait;
    use std::sync::Mutex;
    use std::time::Duration;

    fn dialog(id: u32, title: &str, buttons: &[&str]) -> DialogInfo {
        DialogInfo {
            id,
            app: Some("Editor".into()),
            title: Some(title.into()),
            kind: "dialog".into(),
            bounds: None,
            buttons: buttons.iter().map(|s| s.to_string()).collect(),
            text: vec!["Do you want to save changes?".into()],
            default_button: buttons.first().map(|s| s.to_string()),
            cancel_button: None,
            has_secure_field: false,
        }
    }

    /// A transport that replays one scripted reply and records the request.
    struct Scripted {
        reply: Mutex<Option<Result<(u16, String), String>>>,
        sent: Mutex<Option<Value>>,
    }
    #[async_trait]
    impl mcp_judge::Transport for Scripted {
        async fn post(
            &self,
            _url: &str,
            _key: &str,
            body: &Value,
            _t: Duration,
        ) -> Result<(u16, String), String> {
            *self.sent.lock().unwrap() = Some(body.clone());
            self.reply
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| Err("script exhausted".into()))
        }
    }

    fn judge_with(
        reply: Result<(u16, String), String>,
    ) -> (mcp_judge::Judge, std::sync::Arc<Scripted>) {
        let s = std::sync::Arc::new(Scripted {
            reply: Mutex::new(Some(reply)),
            sent: Mutex::new(None),
        });
        let cfg = mcp_judge::JudgeConfig {
            enabled: true,
            match_threshold: Some(0.6),
            ..mcp_judge::JudgeConfig::default()
        };
        struct Wrap(std::sync::Arc<Scripted>);
        #[async_trait]
        impl mcp_judge::Transport for Wrap {
            async fn post(
                &self,
                url: &str,
                key: &str,
                body: &Value,
                t: Duration,
            ) -> Result<(u16, String), String> {
                self.0.post(url, key, body, t).await
            }
        }
        let j = mcp_judge::Judge::with_transport(cfg, Some("k".into()), Box::new(Wrap(s.clone())));
        (j, s)
    }

    #[test]
    fn candidates_key_each_button_by_dialog_and_survive_duplicate_labels() {
        let ds = [
            dialog(0, "Save", &["Save", "Discard"]),
            dialog(1, "Quit", &["Save"]),
        ];
        let c = button_candidates(&ds);
        assert_eq!(c.len(), 3, "the two 'Save' buttons stay distinct");
        assert!(c.contains_key("0:Save"));
        assert!(c.contains_key("1:Save"));
        assert!(c.contains_key("0:Discard"));
    }

    #[test]
    fn no_buttons_yields_no_candidates() {
        assert!(button_candidates(&[dialog(0, "Empty", &[])]).is_empty());
    }

    #[test]
    fn a_choice_key_splits_back_into_dialog_and_label_even_with_a_colon_in_the_label() {
        assert_eq!(
            parse_button_choice("3:Save As…"),
            (Some(3), "Save As…".into())
        );
        assert_eq!(
            parse_button_choice("0:Time: now"),
            (Some(0), "Time: now".into())
        );
        assert_eq!(parse_button_choice("weird"), (None, "weird".into()));
    }

    #[tokio::test]
    async fn a_confident_pick_names_the_button_and_its_dialog() {
        let ds = [dialog(0, "Save", &["Save", "Discard", "Cancel"])];
        let candidates = button_candidates(&ds);
        let body = r#"{"answers":{
            "pick":{"type":"choice","choice":"0:Save","probabilities":{"0:Save":0.9,"0:Discard":0.05,"0:Cancel":0.05},"confidence":0.9},
            "any":{"type":"noul","noul":0.95}
        }}"#;
        let (j, sent) = judge_with(Ok((200, body.into())));
        let out = rank_dialog_buttons(&j, "save my work", &ds, &candidates)
            .await
            .unwrap();
        assert_eq!(out["button"], "Save");
        assert_eq!(out["dialog"], 0);
        assert_eq!(out["confident"], true);
        // The intent and the dialog text both went to the model as state.
        let s = sent.sent.lock().unwrap().clone().unwrap();
        assert_eq!(s["state"]["intent"], "save my work");
        assert!(s["state"]["dialogs"][0]["text"][0]
            .as_str()
            .unwrap()
            .contains("save changes"));
    }

    #[tokio::test]
    async fn a_pick_no_candidate_really_fits_is_marked_not_confident() {
        let ds = [dialog(0, "Save", &["Save", "Discard"])];
        let candidates = button_candidates(&ds);
        // any_fits below match_threshold (0.6): the model forced a pick.
        let body = r#"{"answers":{
            "pick":{"type":"choice","choice":"0:Save","probabilities":{"0:Save":0.55,"0:Discard":0.45},"confidence":0.51},
            "any":{"type":"noul","noul":0.2}
        }}"#;
        let (j, _) = judge_with(Ok((200, body.into())));
        let out = rank_dialog_buttons(&j, "reboot the machine", &ds, &candidates)
            .await
            .unwrap();
        assert_eq!(out["confident"], false, "any_fits 0.2 < 0.6");
        assert_eq!(out["any_fits"], 0.2);
    }

    #[tokio::test]
    async fn a_judge_failure_surfaces_as_an_error_not_a_wrong_button() {
        let ds = [dialog(0, "Save", &["Save"])];
        let candidates = button_candidates(&ds);
        let (j, _) = judge_with(Ok((500, "boom".into())));
        assert!(rank_dialog_buttons(&j, "save", &ds, &candidates)
            .await
            .is_err());
    }
}

#[cfg(test)]
mod named_app_tests {
    use super::*;
    use crate::backend::{MenuItemInfo, TargetSource, WindowState};
    use mcp_a11y::{BackendError, RawSnapshot, SnapshotRequest};
    use mcp_types::CancelToken;
    use std::sync::Mutex;

    /// Two running applications, an optional pin, and a record of the `app`
    /// each operation was handed. It stands in for no OS: it shows which
    /// application the module asked the backend to act on.
    struct Desk {
        pin: Mutex<Option<TargetInfo>>,
        asked: Mutex<Vec<String>>,
        /// What a name resolves to, when it is not the name itself.
        resolves: Option<(&'static str, &'static str)>,
    }

    impl Desk {
        fn new() -> Self {
            Desk {
                pin: Mutex::new(None),
                asked: Mutex::new(Vec::new()),
                resolves: None,
            }
        }
        fn note(&self, what: String) {
            self.asked.lock().unwrap().push(what);
        }
        fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }
        fn running(name: &str) -> Option<(&'static str, u32)> {
            [("TextEdit", 11), ("Finder", 12), ("Safari", 13)]
                .into_iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
        }
    }

    #[async_trait]
    impl WindowBackend for Desk {
        async fn list_windows(
            &self,
            app: Option<&str>,
        ) -> Result<Vec<crate::backend::WindowInfo>, WindowError> {
            self.note(format!("list_windows {}", app.unwrap_or("-")));
            Ok(Vec::new())
        }
        async fn list_apps(&self) -> Result<Vec<String>, WindowError> {
            Ok(Vec::new())
        }
        async fn launch(&self, _: &str) -> Result<(), WindowError> {
            Ok(())
        }
        async fn close_app(&self, _: &str) -> Result<(), WindowError> {
            Ok(())
        }
        async fn control_window(
            &self,
            app: Option<&str>,
            title: Option<&str>,
            action: WindowAction,
            _: Option<Rect>,
        ) -> Result<(), WindowError> {
            self.note(format!(
                "control_window {} {} {action:?}",
                app.unwrap_or("-"),
                title.unwrap_or("-")
            ));
            Ok(())
        }
        fn control_takes_foreground(&self, _: WindowAction) -> bool {
            false
        }
        async fn menu_open(&self, app: Option<&str>, _: &[String]) -> Result<(), WindowError> {
            self.note(format!("menu_open {}", app.unwrap_or("-")));
            Ok(())
        }
        async fn menu_invoke(&self, app: Option<&str>, _: &[String]) -> Result<(), WindowError> {
            self.note(format!("menu_invoke {}", app.unwrap_or("-")));
            Ok(())
        }
        async fn list_dialogs(
            &self,
            app: Option<&str>,
            _: DialogScope,
        ) -> Result<Vec<crate::backend::DialogInfo>, WindowError> {
            self.note(format!("list_dialogs {}", app.unwrap_or("-")));
            Ok(Vec::new())
        }
        async fn menu_list(
            &self,
            app: Option<&str>,
            _: &[String],
            _: u32,
        ) -> Result<Vec<MenuItemInfo>, WindowError> {
            self.note(format!("menu_list {}", app.unwrap_or("-")));
            Ok(Vec::new())
        }
        async fn focus_app(&self, app: Option<&str>) -> Result<Option<String>, WindowError> {
            self.note(format!("focus_app {}", app.unwrap_or("-")));
            Ok(app.map(str::to_string))
        }
        async fn pin_app(&self, app: &str) -> Result<TargetInfo, WindowError> {
            self.note(format!("pin_app {app}"));
            let (name, pid) = Desk::running(app).ok_or_else(|| {
                WindowError::NotFound(format!("no running application matching '{app}'"))
            })?;
            let t = TargetInfo {
                app: name.to_string(),
                pid: Some(pid),
                from: TargetSource::Named,
            };
            *self.pin.lock().unwrap() = Some(TargetInfo {
                from: TargetSource::Pinned,
                ..t.clone()
            });
            Ok(t)
        }
        async fn describe_target(&self, app: Option<&str>) -> Result<TargetInfo, WindowError> {
            match app {
                Some(a) => {
                    let (name, pid) = Desk::running(a).ok_or_else(|| {
                        WindowError::NotFound(format!("no running application matching '{a}'"))
                    })?;
                    let name = match self.resolves {
                        Some((from, to)) if from == name => to,
                        _ => name,
                    };
                    Ok(TargetInfo {
                        app: name.to_string(),
                        pid: Some(pid),
                        from: TargetSource::Named,
                    })
                }
                None => Ok(self.pin.lock().unwrap().clone().unwrap_or(TargetInfo {
                    app: "Safari".into(),
                    pid: Some(13),
                    from: TargetSource::Frontmost,
                })),
            }
        }
        async fn pinned(&self) -> Option<TargetInfo> {
            self.pin.lock().unwrap().clone()
        }
        fn platform(&self) -> &'static str {
            "desk"
        }
    }

    struct NoTree;
    #[async_trait]
    impl A11yBackend for NoTree {
        async fn snapshot(&self, _: &SnapshotRequest) -> Result<RawSnapshot, BackendError> {
            Err(BackendError::Unsupported("test".into()))
        }
        fn platform(&self) -> &'static str {
            "desk"
        }
    }

    fn module(desk: &Arc<Desk>) -> WindowModule {
        WindowModule::new(
            desk.clone(),
            Arc::new(NoTree),
            vec!["TextEdit".into(), "Finder".into()],
        )
    }

    async fn call(m: &WindowModule, tool: &str, args: Value) -> Envelope {
        m.call(tool, args, &CallCtx::new("t", CancelToken::new()))
            .await
    }

    /// `control_window` used to read the frontmost app whatever `app` said,
    /// so closing a window of one application closed one of another.
    #[tokio::test]
    async fn a_named_app_reaches_the_backend_and_the_result_says_which_app_acted() {
        let desk = Arc::new(Desk::new());
        let m = module(&desk);
        let env = call(
            &m,
            "control_window",
            json!({"app":"textedit","title":"Notes","action":"minimize"}),
        )
        .await;
        assert!(env.ok, "{env:?}");
        assert_eq!(desk.asked(), ["control_window textedit Notes Minimize"]);
        let t = &env.data.unwrap()["target"];
        assert_eq!(t["app"], "TextEdit");
        assert_eq!(t["pid"], 11);
        assert_eq!(t["from"], "named");
    }

    #[tokio::test]
    async fn with_no_app_the_result_says_whether_the_pin_or_the_frontmost_app_was_used() {
        let desk = Arc::new(Desk::new());
        let m = module(&desk);
        let env = call(&m, "menu_list", json!({})).await;
        assert_eq!(env.data.unwrap()["target"]["from"], "frontmost");
        assert!(
            call(&m, "focus_app", json!({"app":"Finder","activate":false}))
                .await
                .ok
        );
        let env = call(&m, "menu_list", json!({})).await;
        let t = env.data.unwrap()["target"].clone();
        assert_eq!(
            (t["app"].as_str(), t["from"].as_str()),
            (Some("Finder"), Some("pinned"))
        );
    }

    /// An application that is not running is an error before the operation,
    /// not a fall-through to whichever app is in front.
    #[tokio::test]
    async fn a_name_that_matches_no_running_app_is_not_found_and_nothing_else_is_acted_on() {
        let desk = Arc::new(Desk::new());
        let m = module(&desk);
        for (tool, args) in [
            ("list_windows", json!({"app":"TextEditt"})),
            (
                "control_window",
                json!({"app":"TextEditt","action":"close"}),
            ),
            (
                "menu_invoke",
                json!({"app":"TextEditt","path":["File","Save"]}),
            ),
            ("menu_list", json!({"app":"TextEditt"})),
            ("handle_dialogs", json!({"app":"TextEditt"})),
            ("focus_app", json!({"app":"TextEditt","activate":false})),
        ] {
            let env = call(&m, tool, args).await;
            let err = env.error.unwrap_or_else(|| panic!("{tool} should fail"));
            // "TextEditt" contains an allowed name, so it passes the list and
            // fails the lookup: the typo is an error, not the app in front.
            assert_eq!(err.code, ErrorCode::NotFound, "{tool}: {err:?}");
            assert!(
                err.message.contains("'TextEditt'"),
                "{tool}: {}",
                err.message
            );
        }
        assert!(desk.asked().is_empty(), "{:?}", desk.asked());
    }

    #[tokio::test]
    async fn an_allowed_name_that_is_not_running_is_not_found_and_names_it() {
        // Finder is allowed; make the lookup fail for it by asking for an
        // allowed substring that matches nothing running.
        let desk = Arc::new(Desk::new());
        let m = WindowModule::new(desk.clone(), Arc::new(NoTree), vec!["Ghost".into()]);
        let env = call(&m, "list_windows", json!({"app":"Ghost"})).await;
        let err = env.error.unwrap();
        assert_eq!(err.code, ErrorCode::NotFound);
        assert!(err.message.contains("'Ghost'"), "{}", err.message);
        assert!(desk.asked().is_empty());
    }

    /// Listing, driving or pinning an application outside `allowed_apps` is
    /// refused the way `launch` always has been, and nothing reaches the OS.
    #[tokio::test]
    async fn every_tool_that_names_an_app_holds_it_to_allowed_apps() {
        let desk = Arc::new(Desk::new());
        let m = module(&desk);
        for (tool, args) in [
            ("list_windows", json!({"app":"Safari"})),
            (
                "control_window",
                json!({"app":"Safari","action":"minimize"}),
            ),
            ("menu_open", json!({"app":"Safari","path":["File"]})),
            (
                "menu_invoke",
                json!({"app":"Safari","path":["File","Save"]}),
            ),
            ("menu_list", json!({"app":"Safari"})),
            ("handle_dialogs", json!({"app":"Safari"})),
            (
                "wait_for",
                json!({"app":"Safari","text":"x","timeout_ms":10}),
            ),
            ("focus_app", json!({"app":"Safari"})),
            ("focus_app", json!({"app":"Safari","activate":false})),
            ("launch", json!({"app":"Safari"})),
        ] {
            let env = call(&m, tool, args.clone()).await;
            let err = env
                .error
                .unwrap_or_else(|| panic!("{tool} {args} was allowed"));
            assert_eq!(err.code, ErrorCode::PolicyDenied, "{tool} {args}");
            assert!(
                err.message.contains("allowed_apps"),
                "{tool}: {}",
                err.message
            );
        }
        assert!(desk.asked().is_empty(), "{:?}", desk.asked());
    }

    /// The allowlist is checked against the application a name resolves to as
    /// well as the name, so a spelling that contains an allowed name cannot
    /// reach one that is not.
    #[tokio::test]
    async fn a_name_is_also_checked_as_the_app_it_resolves_to() {
        let mut desk = Desk::new();
        desk.resolves = Some(("Safari", "Safari"));
        let desk = Arc::new(desk);
        let m = WindowModule::new(desk.clone(), Arc::new(NoTree), vec!["Fari".into()]);
        // The control case: the name and what it resolves to are both covered.
        assert!(call(&m, "list_windows", json!({"app":"Safari"})).await.ok);
        // A name the list covers that resolves to an app it does not is refused.
        let mut desk = Desk::new();
        desk.resolves = Some(("Finder", "Spotlight"));
        let desk = Arc::new(desk);
        let m = module(&desk);
        let env = call(&m, "list_windows", json!({"app":"Finder"})).await;
        assert_eq!(env.error.unwrap().code, ErrorCode::PolicyDenied);
        assert!(desk.asked().is_empty());
    }

    #[tokio::test]
    async fn the_quiet_pin_pins_without_activating_and_the_plain_form_still_activates() {
        let desk = Arc::new(Desk::new());
        let m = module(&desk);
        let env = call(&m, "focus_app", json!({"app":"Finder","activate":false})).await;
        let d = env.data.unwrap();
        assert_eq!(
            (
                d["target"].as_str(),
                d["activated"].as_bool(),
                d["pid"].as_u64()
            ),
            (Some("Finder"), Some(false), Some(12))
        );
        assert_eq!(desk.asked(), ["pin_app Finder"]);
        let env = call(&m, "focus_app", json!({"app":"Finder"})).await;
        assert_eq!(env.data.unwrap()["activated"], true);
        assert_eq!(desk.asked(), ["pin_app Finder", "focus_app Finder"]);
        let env = call(&m, "focus_app", json!({"app":"Finder","activate":"no"})).await;
        assert_eq!(env.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn list_windows_says_whether_it_listed_one_app_or_all_of_them() {
        let desk = Arc::new(Desk::new());
        let m = module(&desk);
        let all = call(&m, "list_windows", json!({})).await.data.unwrap();
        assert_eq!(all["scope"], "all_apps");
        assert!(all.get("target").is_none());
        let one = call(&m, "list_windows", json!({"app":"Finder"}))
            .await
            .data
            .unwrap();
        assert_eq!(one["target"]["app"], "Finder");
        assert!(one.get("scope").is_none());
        // Pinned, and naming nothing: the pin is what was listed.
        call(&m, "focus_app", json!({"app":"TextEdit","activate":false})).await;
        let pinned = call(&m, "list_windows", json!({})).await.data.unwrap();
        assert_eq!(pinned["target"]["from"], "pinned");
    }

    #[test]
    fn a_window_state_serialises_as_a_lowercase_word() {
        assert_eq!(
            serde_json::to_value(WindowState::Offscreen).unwrap(),
            "offscreen"
        );
    }
}
