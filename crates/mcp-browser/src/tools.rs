use std::sync::Arc;

use async_trait::async_trait;
use mcp_types::{
    CallCtx, Category, Envelope, ErrorCode, ImageContent, Tier, ToolDescriptor, ToolModule,
};
use serde_json::{json, Value};

use crate::backend::{BrowserBackend, BrowserError};
use crate::cdp::DialogPolicy;

/// The `browser` CDP engine (`docs/planning.md` §5.11): DOM-level control of a
/// Chromium browser attached over the Chrome DevTools Protocol.
pub struct BrowserModule {
    backend: Arc<dyn BrowserBackend>,
}

impl BrowserModule {
    pub fn new(backend: Arc<dyn BrowserBackend>) -> Self {
        BrowserModule { backend }
    }
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

// Envelope is intentionally large (carries an optional image); it is the Err
// type here only as a control-flow shortcut for missing args.
#[allow(clippy::result_large_err)]
fn require<'a>(args: &'a Value, key: &str, tool: &str) -> Result<&'a str, Envelope> {
    str_arg(args, key)
        .ok_or_else(|| Envelope::fail(tool, ErrorCode::InvalidArgs, format!("missing '{key}'")))
}

fn browser_err(tool: &str, e: BrowserError) -> Envelope {
    let (code, msg) = match e {
        BrowserError::PermissionDenied(m) => (ErrorCode::PermDenied, m),
        BrowserError::NotFound(m) => (ErrorCode::NotFound, m),
        BrowserError::Unsupported(m) => (ErrorCode::UnsupportedOs, m),
        BrowserError::Timeout(m) => (ErrorCode::Timeout, m),
        BrowserError::Failed(m) => (ErrorCode::ActionFailed, m),
    };
    Envelope::fail(tool, code, msg)
}

fn result(tool: &str, r: Result<Value, BrowserError>) -> Envelope {
    match r {
        Ok(v) => Envelope::ok(tool, v),
        Err(e) => browser_err(tool, e),
    }
}

impl BrowserModule {
    async fn connect(&self, args: &Value) -> Envelope {
        let attach_port = args
            .get("attach")
            .and_then(|a| a.get("port"))
            .and_then(Value::as_u64)
            .map(|p| p as u16);
        let launch = args.get("launch").cloned();
        result(
            "browser_connect",
            self.backend.connect(attach_port, launch).await,
        )
    }

    async fn disconnect(&self, args: &Value) -> Envelope {
        let Some(browser_id) = args.get("browser_id").and_then(Value::as_u64) else {
            return Envelope::fail(
                "browser_disconnect",
                ErrorCode::InvalidArgs,
                "missing 'browser_id'",
            );
        };
        let kill = args.get("kill").and_then(Value::as_bool).unwrap_or(false);
        result(
            "browser_disconnect",
            self.backend.disconnect(browser_id as u32, kill).await,
        )
    }

    async fn tabs(&self, args: &Value) -> Envelope {
        let Some(browser_id) = args.get("browser_id").and_then(Value::as_u64) else {
            return Envelope::fail(
                "browser_tabs",
                ErrorCode::InvalidArgs,
                "missing 'browser_id'",
            );
        };
        let action = str_arg(args, "action").unwrap_or("list");
        let target = str_arg(args, "target_id");
        let url = str_arg(args, "url");
        result(
            "browser_tabs",
            self.backend
                .tabs(browser_id as u32, action, target, url)
                .await,
        )
    }

    async fn navigate(&self, args: &Value) -> Envelope {
        let target = match require(args, "target_id", "browser_navigate") {
            Ok(t) => t,
            Err(e) => return e,
        };
        let action = str_arg(args, "action").unwrap_or("goto");
        result(
            "browser_navigate",
            self.backend
                .navigate(target, action, str_arg(args, "url"))
                .await,
        )
    }

    async fn snapshot(&self, args: &Value) -> Envelope {
        let target = match require(args, "target_id", "browser_snapshot") {
            Ok(t) => t,
            Err(e) => return e,
        };
        let mode = str_arg(args, "mode").unwrap_or("dom");
        result(
            "browser_snapshot",
            self.backend
                .snapshot(target, mode, str_arg(args, "root_selector"))
                .await,
        )
    }

    async fn query(&self, args: &Value) -> Envelope {
        let target = match require(args, "target_id", "browser_query") {
            Ok(t) => t,
            Err(e) => return e,
        };
        let by = str_arg(args, "by").unwrap_or("css");
        let q = match require(args, "query", "browser_query") {
            Ok(q) => q,
            Err(e) => return e,
        };
        let all = args.get("all").and_then(Value::as_bool).unwrap_or(false);
        result(
            "browser_query",
            self.backend.query(target, by, q, all).await,
        )
    }

    async fn act(&self, args: &Value) -> Envelope {
        let target = match require(args, "target_id", "browser_act") {
            Ok(t) => t,
            Err(e) => return e,
        };
        let node_ref = match require(args, "ref", "browser_act") {
            Ok(r) => r,
            Err(e) => return e,
        };
        let action = str_arg(args, "action").unwrap_or("click");
        result(
            "browser_act",
            self.backend
                .act(target, node_ref, action, str_arg(args, "value"))
                .await,
        )
    }

    async fn wait(&self, args: &Value) -> Envelope {
        let target = match require(args, "target_id", "browser_wait") {
            Ok(t) => t,
            Err(e) => return e,
        };
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(10_000);
        // exactly one of selector | navigation | network_idle
        let (cond, arg) = if let Some(sel) = str_arg(args, "selector") {
            ("selector", Some(sel))
        } else if args.get("navigation").is_some() {
            ("navigation", None)
        } else if args.get("network_idle").is_some() {
            ("network_idle", None)
        } else {
            return Envelope::fail(
                "browser_wait",
                ErrorCode::InvalidArgs,
                "provide one of 'selector', 'navigation', or 'network_idle'",
            );
        };
        result(
            "browser_wait",
            self.backend.wait(target, cond, arg, timeout_ms).await,
        )
    }

    async fn screenshot(&self, args: &Value) -> Envelope {
        let target = match require(args, "target_id", "browser_screenshot") {
            Ok(t) => t,
            Err(e) => return e,
        };
        match self.backend.screenshot(target, str_arg(args, "ref")).await {
            Ok(shot) => Envelope::ok_image(
                "browser_screenshot",
                json!({ "width": shot.width, "height": shot.height }),
                ImageContent {
                    mime_type: "image/png".into(),
                    base64: shot.base64,
                },
            ),
            Err(e) => browser_err("browser_screenshot", e),
        }
    }

    async fn eval(&self, args: &Value) -> Envelope {
        let target = match require(args, "target_id", "browser_eval") {
            Ok(t) => t,
            Err(e) => return e,
        };
        let expr = match require(args, "expression", "browser_eval") {
            Ok(e) => e,
            Err(e) => return e,
        };
        result("browser_eval", self.backend.eval(target, expr).await)
    }

    async fn dialog(&self, args: &Value) -> Envelope {
        let tool = "browser_dialog";
        let target = match require(args, "target_id", tool) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let policy = match str_arg(args, "policy") {
            None => None,
            Some("dismiss") => Some(DialogPolicy::Dismiss),
            Some("accept") => Some(DialogPolicy::Accept(
                str_arg(args, "prompt_text").map(str::to_string),
            )),
            Some(other) => {
                return Envelope::fail_with(
                    tool,
                    ErrorCode::InvalidArgs,
                    format!("unknown policy '{other}'"),
                    "use 'dismiss' (cancel the dialog) or 'accept' (confirm it)",
                )
            }
        };
        result(tool, self.backend.dialog(target, policy).await)
    }

    async fn network(&self, args: &Value) -> Envelope {
        let target = match require(args, "target_id", "browser_network") {
            Ok(t) => t,
            Err(e) => return e,
        };
        let action = str_arg(args, "action").unwrap_or("log");
        result(
            "browser_network",
            self.backend
                .network(
                    target,
                    action,
                    str_arg(args, "filter"),
                    args.get("headers").cloned(),
                    args.get("duration_ms").and_then(Value::as_u64),
                )
                .await,
        )
    }

    async fn cookies(&self, args: &Value) -> Envelope {
        let target = match require(args, "target_id", "browser_cookies") {
            Ok(t) => t,
            Err(e) => return e,
        };
        let action = str_arg(args, "action").unwrap_or("get");
        result(
            "browser_cookies",
            self.backend
                .cookies(target, action, args.get("cookie").cloned())
                .await,
        )
    }
}

#[async_trait]
impl ToolModule for BrowserModule {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        let obj = |props: Value, required: Value| json!({ "type": "object", "properties": props, "required": required });
        vec![
            ToolDescriptor::new(
                "browser_connect",
                Category::Browser,
                Tier::Standard,
                "Attach to a Chromium browser started with --remote-debugging-port, or launch a dedicated instance.",
                obj(
                    json!({
                        "attach": { "type": "object", "properties": { "port": { "type": "integer" } } },
                        "launch": { "type": "object", "properties": {
                            "port": { "type": "integer" },
                            "headless": { "type": "boolean" },
                            "user_data_dir": { "type": "string" }
                        } }
                    }),
                    json!([]),
                ),
            ),
            ToolDescriptor::new(
                "browser_disconnect",
                Category::Browser,
                Tier::Standard,
                "Disconnect from a browser. With kill=true, also stop a browser this session launched and delete the temporary profile it created (attached browsers are never killed).",
                obj(
                    json!({
                        "browser_id": { "type": "integer" },
                        "kill": { "type": "boolean", "description": "stop the process; only valid for a browser agentctl launched" }
                    }),
                    json!(["browser_id"]),
                ),
            ),
            ToolDescriptor::new(
                "browser_tabs",
                Category::Browser,
                Tier::Standard,
                "List/open/activate/close tabs (targets) of a connected browser.",
                obj(
                    json!({
                        "browser_id": { "type": "integer" },
                        "action": { "type": "string", "enum": ["list", "open", "activate", "close"] },
                        "target_id": { "type": "string" },
                        "url": { "type": "string" }
                    }),
                    json!(["browser_id", "action"]),
                ),
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_navigate",
                Category::Browser,
                Tier::Standard,
                "Navigate a tab: goto a url, or go back/forward/reload.",
                obj(
                    json!({
                        "target_id": { "type": "string" },
                        "action": { "type": "string", "enum": ["goto", "back", "forward", "reload"] },
                        "url": { "type": "string" }
                    }),
                    json!(["target_id", "action"]),
                ),
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_snapshot",
                Category::Browser,
                Tier::Read,
                "Flatten a page into interactable node refs (dom/accessibility) or raw text. The web equivalent of get_ui_tree.",
                obj(
                    json!({
                        "target_id": { "type": "string" },
                        "mode": { "type": "string", "enum": ["dom", "accessibility", "text"] },
                        "root_selector": { "type": "string" }
                    }),
                    json!(["target_id"]),
                ),
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_query",
                Category::Browser,
                Tier::Read,
                "Resolve node ref(s) by css selector, xpath, or visible text.",
                obj(
                    json!({
                        "target_id": { "type": "string" },
                        "by": { "type": "string", "enum": ["css", "xpath", "text"] },
                        "query": { "type": "string" },
                        "all": { "type": "boolean" }
                    }),
                    json!(["target_id", "query"]),
                ),
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_act",
                Category::Browser,
                Tier::Standard,
                "Act on a DOM node ref: click, type, select, hover, focus, scroll_into_view, submit.",
                obj(
                    json!({
                        "target_id": { "type": "string" },
                        "ref": { "type": "string" },
                        "action": { "type": "string", "enum": ["click", "type", "select", "hover", "focus", "scroll_into_view", "submit"] },
                        "value": { "type": "string" }
                    }),
                    json!(["target_id", "ref", "action"]),
                ),
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_wait",
                Category::Browser,
                Tier::Read,
                "Wait for a settle signal: a selector to appear, navigation to complete, or the network to idle.",
                obj(
                    json!({
                        "target_id": { "type": "string" },
                        "selector": { "type": "string" },
                        "navigation": { "type": "boolean" },
                        "network_idle": { "type": "boolean" },
                        "timeout_ms": { "type": "integer" }
                    }),
                    json!(["target_id"]),
                ),
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_screenshot",
                Category::Browser,
                Tier::Read,
                "Capture a PNG of the page (or a single element by ref).",
                obj(
                    json!({
                        "target_id": { "type": "string" },
                        "ref": { "type": "string" }
                    }),
                    json!(["target_id"]),
                ),
            ),
            ToolDescriptor::new(
                "browser_eval",
                Category::Browser,
                Tier::Dangerous,
                "Evaluate arbitrary JavaScript in the page context; result is JSON-serialized. Arbitrary code execution.",
                obj(
                    json!({
                        "target_id": { "type": "string" },
                        "expression": { "type": "string" }
                    }),
                    json!(["target_id", "expression"]),
                ),
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_dialog",
                Category::Browser,
                Tier::Standard,
                "Inspect and control how the page's JavaScript dialogs (alert/confirm/prompt/\
                 beforeunload) are answered. They are answered automatically — an unanswered \
                 dialog blocks the tab — and dismissed by default; call with policy='accept' \
                 only when confirming is what you actually intend. Omit 'policy' to read the \
                 current setting and the dialogs seen so far.",
                json!({
                    "type": "object",
                    "properties": {
                        "target_id": { "type": "string" },
                        "policy": { "type": "string", "enum": ["dismiss", "accept"] },
                        "prompt_text": { "type": "string", "description": "text supplied to prompt() when accepting" }
                    },
                    "required": ["target_id"]
                }),
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_network",
                Category::Browser,
                Tier::Dangerous,
                "Network control. log: record requests and responses for a bounded window \
                 (URLs, methods, statuses — header values and cookies are deliberately omitted). \
                 intercept: block URL patterns via headers.block. set_headers: extra HTTP headers.",
                obj(
                    json!({
                        "target_id": { "type": "string" },
                        "action": { "type": "string", "enum": ["log", "intercept", "set_headers"] },
                        "filter": { "type": "string", "description": "substring filter for log rows" },
                        "duration_ms": { "type": "integer", "description": "log window, 100-30000" },
                        "headers": {
                            "type": "object",
                            "description": "set_headers: the headers. intercept: { block: [url patterns] }"
                        }
                    }),
                    json!(["target_id", "action"]),
                ),
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_cookies",
                Category::Browser,
                Tier::Dangerous,
                "Cookie access: get (values redacted), set, or clear.",
                obj(
                    json!({
                        "target_id": { "type": "string" },
                        "action": { "type": "string", "enum": ["get", "set", "clear"] },
                        "cookie": { "type": "object" }
                    }),
                    json!(["target_id", "action"]),
                ),
            ),
        ]
    }

    /// Answering a page's own confirmation on the user's behalf is a decision,
    /// not plumbing: `confirm("Delete this account?")` becomes "yes". Dismissal
    /// (the default) needs no approval because it is the null answer.
    fn consent_prompt(&self, name: &str, args: &Value) -> Option<String> {
        if name != "browser_dialog" {
            return None;
        }
        if args.get("policy").and_then(Value::as_str) != Some("accept") {
            return None;
        }
        Some("Automatically ACCEPT JavaScript dialogs in this tab? Any confirm() the page raises will be answered 'yes' without further prompting.".to_string())
    }

    /// Stop browsers this session launched. Without this, a `serve` that ends
    /// leaves a headless Chrome and its profile directory behind for good.
    fn shutdown(&self) {
        self.backend.shutdown();
    }

    async fn call(&self, name: &str, args: Value, _ctx: &CallCtx) -> Envelope {
        match name {
            "browser_connect" => self.connect(&args).await,
            "browser_disconnect" => self.disconnect(&args).await,
            "browser_tabs" => self.tabs(&args).await,
            "browser_navigate" => self.navigate(&args).await,
            "browser_snapshot" => self.snapshot(&args).await,
            "browser_query" => self.query(&args).await,
            "browser_act" => self.act(&args).await,
            "browser_wait" => self.wait(&args).await,
            "browser_screenshot" => self.screenshot(&args).await,
            "browser_eval" => self.eval(&args).await,
            "browser_dialog" => self.dialog(&args).await,
            "browser_network" => self.network(&args).await,
            "browser_cookies" => self.cookies(&args).await,
            other => Envelope::fail(other, ErrorCode::InvalidArgs, "unknown tool"),
        }
    }
}
