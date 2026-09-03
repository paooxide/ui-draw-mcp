use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use mcp_a11y::{flatten, A11yBackend, FlattenConfig, SnapshotRequest};
use mcp_types::{CallCtx, Category, Envelope, ErrorCode, Tier, ToolDescriptor, ToolModule};
use serde_json::{json, Value};

use crate::backend::{DialogScope, Rect, WindowAction, WindowBackend, WindowError};

/// The `window` engine: window/app/menu control and the `wait_for` settle
/// primitive. `wait_for` re-observes via the a11y backend (text/element) or the
/// window backend (window).
pub struct WindowModule {
    backend: Arc<dyn WindowBackend>,
    a11y: Arc<dyn A11yBackend>,
    allowed_apps: Vec<String>,
}

impl WindowModule {
    pub fn new(
        backend: Arc<dyn WindowBackend>,
        a11y: Arc<dyn A11yBackend>,
        allowed_apps: Vec<String>,
    ) -> Self {
        WindowModule {
            backend,
            a11y,
            allowed_apps,
        }
    }

    fn app_allowed(&self, app: &str) -> bool {
        let lower = app.to_ascii_lowercase();
        self.allowed_apps
            .iter()
            .any(|a| lower.contains(&a.to_ascii_lowercase()))
    }

    async fn list_windows(&self, args: &Value) -> Envelope {
        match self
            .backend
            .list_windows(str_arg(args, "app").as_deref())
            .await
        {
            Ok(ws) => Envelope::ok("list_windows", json!({ "windows": ws })),
            Err(e) => win_err("list_windows", e),
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
        match self.backend.launch(&app).await {
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
        match self
            .backend
            .control_window(
                str_arg(args, "app").as_deref(),
                str_arg(args, "title").as_deref(),
                action,
                bounds,
            )
            .await
        {
            Ok(()) => Envelope::ok("control_window", json!({ "ok": true })),
            Err(e) => win_err("control_window", e),
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
        let res = if invoke {
            self.backend.menu_invoke(app.as_deref(), &path).await
        } else {
            self.backend.menu_open(app.as_deref(), &path).await
        };
        match res {
            Ok(()) => Envelope::ok(tool, json!({ "ok": true })),
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
        match self
            .backend
            .list_dialogs(str_arg(args, "app").as_deref(), scope)
            .await
        {
            Ok(ds) => {
                // A password prompt is a hand-back-to-the-human signal, not
                // something to fill in: surface it at the top level so it
                // cannot be missed in a list.
                let credential = ds.iter().any(|d| d.has_secure_field);
                Envelope::ok(
                    tool,
                    json!({ "dialogs": ds, "count": ds.len(), "credential_prompt": credential }),
                )
            }
            Err(e) => win_err(tool, e),
        }
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
        match self
            .backend
            .menu_list(str_arg(args, "app").as_deref(), &path, depth)
            .await
        {
            Ok(items) => Envelope::ok(tool, json!({ "items": items, "count": items.len() })),
            Err(e) => win_err(tool, e),
        }
    }

    async fn wait_for(&self, args: &Value) -> Envelope {
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(5000)
            .min(30_000);
        let text = str_arg(args, "text");
        let window = str_arg(args, "window");
        let element = str_arg(args, "element");
        if text.is_none() && window.is_none() && element.is_none() {
            return Envelope::fail(
                "wait_for",
                ErrorCode::InvalidArgs,
                "need one of text|window|element",
            );
        }
        let app = str_arg(args, "app");
        let start = Instant::now();
        loop {
            let matched = if let Some(t) = &window {
                self.window_matches(app.as_deref(), t).await
            } else if let Some(t) = &text {
                self.tree_contains(app.as_deref(), t).await
            } else if let Some(t) = &element {
                self.tree_contains(app.as_deref(), t).await
            } else {
                false
            };
            if matched {
                return Envelope::ok("wait_for", json!({ "ok": true }));
            }
            if start.elapsed().as_millis() as u64 >= timeout_ms {
                return Envelope::fail(
                    "wait_for",
                    ErrorCode::Timeout,
                    "condition not met before timeout",
                );
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn window_matches(&self, app: Option<&str>, title: &str) -> bool {
        self.backend
            .list_windows(app)
            .await
            .map(|ws| {
                ws.iter()
                    .any(|w| w.title.as_deref().is_some_and(|t| t.contains(title)))
            })
            .unwrap_or(false)
    }

    async fn tree_contains(&self, app: Option<&str>, needle: &str) -> bool {
        let req = SnapshotRequest {
            app: app.map(str::to_string),
            ..Default::default()
        };
        match self.a11y.snapshot(&req).await {
            Ok(raw) => {
                let f = flatten(
                    &raw.root,
                    raw.app.as_deref(),
                    raw.window.as_deref(),
                    "wait",
                    &FlattenConfig::default(),
                );
                f.text.contains(needle)
            }
            Err(_) => false,
        }
    }
}

impl WindowModule {
    /// Pin (or release) the session's application target.
    async fn focus_app(&self, args: &Value) -> Envelope {
        let app = str_arg(args, "app");
        if let Some(a) = app.as_deref() {
            if !self.app_allowed(a) {
                return Envelope::fail(
                    "focus_app",
                    ErrorCode::PolicyDenied,
                    format!("app '{a}' is not in policy.allowed_apps"),
                );
            }
        }
        match self.backend.focus_app(app.as_deref()).await {
            Ok(Some(name)) => Envelope::ok("focus_app", json!({ "target": name })),
            Ok(None) => Envelope::ok("focus_app", json!({ "target": null, "released": true })),
            Err(e) => win_err("focus_app", e),
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
                "List windows of an app (or the focused app).",
                json!({"type":"object","properties":{"app":{"type":"string"}},"required":[]}),
            ),
            ToolDescriptor::new(
                "focus_app",
                Category::Window,
                Tier::Standard,
                "Pin this session to an application: bring it forward and make every later \
                 get_ui_tree/action target it instead of whatever happens to be frontmost. \
                 Call once before driving an app. Omit 'app' to release the pin.",
                json!({"type":"object","properties":{"app":{"type":"string"}},"required":[]}),
            ),
            ToolDescriptor::new(
                "list_apps",
                Category::Window,
                Tier::Read,
                "List running applications.",
                json!({"type":"object","properties":{},"required":[]}),
            ),
            ToolDescriptor::new(
                "launch",
                Category::Window,
                Tier::Standard,
                "Launch or bring forward an application by name (allowlist-gated).",
                json!({"type":"object","properties":{"app":{"type":"string"}},"required":["app"]}),
            ),
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
                "Focus/move/resize/minimize/maximize/restore/close a window.",
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
                 Press a button with ui_action on the matching ref from get_ui_tree.",
                json!({"type":"object","properties":{
                    "app":{"type":"string"},
                    "scope":{"type":"string","enum":["app","system"]}
                },"required":[]}),
            ),
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
            ),
            ToolDescriptor::new(
                "wait_for",
                Category::Window,
                Tier::Read,
                "Block until text appears, a window appears, or an element exists (settle signal).",
                json!({"type":"object","properties":{
                    "text":{"type":"string"},"window":{"type":"string"},"element":{"type":"string"},
                    "app":{"type":"string"},"timeout_ms":{"type":"integer"}},"required":[]}),
            ),
        ]
    }

    async fn call(&self, name: &str, args: Value, _ctx: &CallCtx) -> Envelope {
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
            "wait_for" => self.wait_for(&args).await,
            "focus_app" => self.focus_app(&args).await,
            other => Envelope::fail(other, ErrorCode::InvalidArgs, "unknown tool"),
        }
    }
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
    };
    Envelope::fail(tool, code, msg)
}
