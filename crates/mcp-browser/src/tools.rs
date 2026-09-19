use std::sync::Arc;

use async_trait::async_trait;
use mcp_types::{
    CallCtx, Category, Envelope, ErrorCode, ImageContent, Tier, ToolDescriptor, ToolError,
    ToolModule,
};
use serde_json::{json, Value};

use crate::backend::{BrowserBackend, BrowserError};
use crate::cdp::DialogPolicy;

/// The `browser` CDP engine: DOM-level control of a
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
        // Either a ref from a prior snapshot/query, or a selector resolved in
        // the same call (one round trip instead of query-then-act).
        let locator = if let Some(r) = str_arg(args, "ref") {
            crate::backend::Locator::Ref(r)
        } else if let Some(q) = str_arg(args, "query") {
            crate::backend::Locator::Selector {
                by: str_arg(args, "by").unwrap_or("css"),
                query: q,
            }
        } else {
            return Envelope::fail_with(
                "browser_act",
                ErrorCode::InvalidArgs,
                "need 'ref' (from browser_query/snapshot) or 'query' (with optional 'by')",
                "pass ref, or query plus by=css|xpath|text",
            );
        };
        let action = str_arg(args, "action").unwrap_or("click");
        result(
            "browser_act",
            self.backend
                .act(target, locator, action, str_arg(args, "value"))
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

    async fn capture(&self, args: &Value) -> Envelope {
        let target = match require(args, "target_id", "browser_capture") {
            Ok(t) => t,
            Err(e) => return e,
        };
        let action = str_arg(args, "action").unwrap_or("read");
        result(
            "browser_capture",
            self.backend.capture(target, action, args).await,
        )
    }

    async fn assert(&self, args: &Value) -> Envelope {
        let target = match require(args, "target_id", "browser_assert") {
            Ok(t) => t,
            Err(e) => return e,
        };
        match self.backend.assert(target, args).await {
            Ok(v) => {
                let passed = v.get("passed").and_then(Value::as_bool) == Some(true);
                if passed {
                    Envelope::ok("browser_assert", v)
                } else {
                    // A failed assertion is an error the harness must see, but
                    // the per-check detail rides along in `data`.
                    Envelope {
                        ok: false,
                        tool: "browser_assert".into(),
                        data: Some(v),
                        error: Some(ToolError {
                            code: ErrorCode::ActionFailed,
                            message: "assertion failed".into(),
                            suggestion: Some("see data.checks for which clause failed".into()),
                        }),
                        image: None,
                    }
                }
            }
            Err(e) => browser_err("browser_assert", e),
        }
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
                "Act on a DOM node: click, type, select, hover, focus, scroll_into_view, submit. \
                 Target it with 'ref' (from browser_query/snapshot) or, in one call, with \
                 'query' plus 'by' (css/xpath/text) to resolve and act without a separate query.",
                obj(
                    json!({
                        "target_id": { "type": "string" },
                        "ref": { "type": "string", "description": "a ref from browser_query/snapshot" },
                        "by": { "type": "string", "enum": ["css", "xpath", "text"], "description": "how to read 'query' (default css); used when no 'ref'" },
                        "query": { "type": "string", "description": "selector to resolve and act on in one call, instead of 'ref'" },
                        "action": { "type": "string", "enum": ["click", "type", "select", "hover", "focus", "scroll_into_view", "submit"] },
                        "value": { "type": "string" }
                    }),
                    json!(["target_id", "action"]),
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
                 beforeunload) are answered. They are answered automatically: an unanswered \
                 dialog blocks the tab: and dismissed by default; call with policy='accept' \
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
                 (URLs, methods, statuses: header values and cookies are deliberately omitted). \
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
            ToolDescriptor::new(
                "browser_capture",
                Category::Browser,
                Tier::Dangerous,
                "Regression-test capture. 'start' installs a page hook (persists across \
                 navigations) that records fetch/XHR calls with request+response bodies and \
                 console errors/uncaught exceptions. 'read' returns them ('only_errors' keeps \
                 failed requests; 'filter' is a substring). 'clear' empties the buffers. Bodies \
                 can contain secrets, so this is off unless enabled.",
                obj(
                    json!({
                        "target_id": { "type": "string" },
                        "action": { "type": "string", "enum": ["start", "read", "clear"] },
                        "only_errors": { "type": "boolean", "description": "read: keep only non-2xx / failed requests" },
                        "filter": { "type": "string", "description": "read: substring filter over rows" }
                    }),
                    json!(["target_id"]),
                ),
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_assert",
                Category::Browser,
                Tier::Read,
                "Settle (optional) then check the page in one call; returns {passed, checks} and \
                 errors when it fails. Clauses: text/not_text (in page text), url (substring), \
                 selector (+min_count), no_console_errors and no_failed_requests (need \
                 browser_capture started). Settle first with wait_selector or \
                 wait_network_idle.",
                obj(
                    json!({
                        "target_id": { "type": "string" },
                        "text": { "type": "string", "description": "assert this text is present" },
                        "not_text": { "type": "string", "description": "assert this text is absent" },
                        "url": { "type": "string", "description": "assert the URL contains this" },
                        "selector": { "type": "string", "description": "assert this css selector matches" },
                        "min_count": { "type": "integer", "description": "selector must match at least this many (default 1)" },
                        "no_console_errors": { "type": "boolean", "description": "assert no captured console errors (needs browser_capture)" },
                        "no_failed_requests": { "type": "boolean", "description": "assert no captured non-2xx/failed requests (needs browser_capture)" },
                        "wait_selector": { "type": "string", "description": "settle: wait for this selector first" },
                        "wait_network_idle": { "type": "boolean", "description": "settle: wait for network idle first" },
                        "timeout_ms": { "type": "integer", "description": "settle timeout (default 8000)" }
                    }),
                    json!(["target_id"]),
                ),
            ).untrusted_output(),
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
            "browser_capture" => self.capture(&args).await,
            "browser_assert" => self.assert(&args).await,
            other => Envelope::fail(other, ErrorCode::InvalidArgs, "unknown tool"),
        }
    }
}

#[cfg(test)]
mod act_tests {
    use super::*;
    use crate::backend::{Locator, Shot};
    use std::sync::Mutex;

    /// Records how `act` was asked to locate the element; everything else is a
    /// no-op error, since these tests only exercise the tool-layer wiring.
    #[derive(Default)]
    struct Recorder {
        acts: Mutex<Vec<String>>,
        captures: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl BrowserBackend for Recorder {
        async fn connect(&self, _p: Option<u16>, _l: Option<Value>) -> Result<Value, BrowserError> {
            Err(BrowserError::Failed("n/a".into()))
        }
        async fn disconnect(&self, _b: u32, _k: bool) -> Result<Value, BrowserError> {
            Err(BrowserError::Failed("n/a".into()))
        }
        async fn tabs(&self, _b: u32, _a: &str, _t: Option<&str>, _u: Option<&str>) -> Result<Value, BrowserError> {
            Err(BrowserError::Failed("n/a".into()))
        }
        async fn navigate(&self, _t: &str, _a: &str, _u: Option<&str>) -> Result<Value, BrowserError> {
            Err(BrowserError::Failed("n/a".into()))
        }
        async fn snapshot(&self, _t: &str, _m: &str, _r: Option<&str>) -> Result<Value, BrowserError> {
            Err(BrowserError::Failed("n/a".into()))
        }
        async fn query(&self, _t: &str, _by: &str, _q: &str, _a: bool) -> Result<Value, BrowserError> {
            Err(BrowserError::Failed("n/a".into()))
        }
        async fn act(&self, _t: &str, locator: Locator<'_>, action: &str, _v: Option<&str>) -> Result<Value, BrowserError> {
            let desc = match locator {
                Locator::Ref(r) => format!("ref:{r}"),
                Locator::Selector { by, query } => format!("sel:{by}:{query}"),
            };
            self.acts.lock().unwrap().push(desc);
            Ok(json!({ "ok": true, "action": action }))
        }
        async fn wait(&self, _t: &str, _c: &str, _a: Option<&str>, _ms: u64) -> Result<Value, BrowserError> {
            Err(BrowserError::Failed("n/a".into()))
        }
        async fn screenshot(&self, _t: &str, _r: Option<&str>) -> Result<Shot, BrowserError> {
            Err(BrowserError::Failed("n/a".into()))
        }
        async fn eval(&self, _t: &str, _e: &str) -> Result<Value, BrowserError> {
            Err(BrowserError::Failed("n/a".into()))
        }
        async fn network(&self, _t: &str, _a: &str, _f: Option<&str>, _h: Option<Value>, _d: Option<u64>) -> Result<Value, BrowserError> {
            Err(BrowserError::Failed("n/a".into()))
        }
        async fn dialog(&self, _t: &str, _p: Option<DialogPolicy>) -> Result<Value, BrowserError> {
            Err(BrowserError::Failed("n/a".into()))
        }
        async fn cookies(&self, _t: &str, _a: &str, _c: Option<Value>) -> Result<Value, BrowserError> {
            Err(BrowserError::Failed("n/a".into()))
        }
        async fn capture(&self, _t: &str, action: &str, _o: &Value) -> Result<Value, BrowserError> {
            self.captures.lock().unwrap().push(action.to_string());
            Ok(json!({ "ok": true, "action": action }))
        }
        async fn assert(&self, _t: &str, spec: &Value) -> Result<Value, BrowserError> {
            // Echo a passed/failed result driven by a test-only `_pass` flag.
            let passed = spec.get("_pass").and_then(Value::as_bool).unwrap_or(true);
            Ok(json!({ "passed": passed, "checks": [{"name":"x","ok":passed}] }))
        }
    }

    fn module() -> (BrowserModule, Arc<Recorder>) {
        let rec = Arc::new(Recorder::default());
        (BrowserModule::new(rec.clone()), rec)
    }

    #[tokio::test]
    async fn a_ref_locates_by_ref() {
        let (m, rec) = module();
        let e = m.act(&json!({"target_id":"T","ref":"/html/body[1]/button[1]","action":"click"})).await;
        assert!(e.ok, "{e:?}");
        assert_eq!(rec.acts.lock().unwrap()[0], "ref:/html/body[1]/button[1]");
    }

    #[tokio::test]
    async fn a_query_locates_by_selector_in_one_call() {
        let (m, rec) = module();
        let e = m.act(&json!({"target_id":"T","by":"text","query":"Login","action":"click"})).await;
        assert!(e.ok, "{e:?}");
        // No separate browser_query was needed: the selector reached act directly.
        assert_eq!(rec.acts.lock().unwrap()[0], "sel:text:Login");
    }

    #[tokio::test]
    async fn a_query_defaults_to_css_when_by_is_omitted() {
        let (m, rec) = module();
        let e = m.act(&json!({"target_id":"T","query":"#save","action":"click"})).await;
        assert!(e.ok, "{e:?}");
        assert_eq!(rec.acts.lock().unwrap()[0], "sel:css:#save");
    }

    #[tokio::test]
    async fn neither_ref_nor_query_is_an_invalid_argument_and_never_calls_the_backend() {
        let (m, rec) = module();
        let e = m.act(&json!({"target_id":"T","action":"click"})).await;
        assert!(!e.ok);
        assert_eq!(e.error.unwrap().code, ErrorCode::InvalidArgs);
        assert!(rec.acts.lock().unwrap().is_empty(), "backend must not be called");
    }

    #[tokio::test]
    async fn capture_routes_the_action_and_defaults_to_read() {
        let (m, rec) = module();
        assert!(m.capture(&json!({"target_id":"T","action":"start"})).await.ok);
        assert!(m.capture(&json!({"target_id":"T"})).await.ok); // default
        assert_eq!(*rec.captures.lock().unwrap(), vec!["start", "read"]);
    }

    #[tokio::test]
    async fn capture_without_a_target_is_an_invalid_argument() {
        let (m, rec) = module();
        let e = m.capture(&json!({"action":"read"})).await;
        assert!(!e.ok);
        assert_eq!(e.error.unwrap().code, ErrorCode::InvalidArgs);
        assert!(rec.captures.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_passing_assert_is_ok_and_a_failing_one_is_an_error_carrying_the_checks() {
        let (m, _) = module();
        let ok = m.assert(&json!({"target_id":"T","_pass":true})).await;
        assert!(ok.ok);
        assert_eq!(ok.data.unwrap()["passed"], true);

        let bad = m.assert(&json!({"target_id":"T","_pass":false})).await;
        assert!(!bad.ok, "a failed assertion must surface as an error");
        assert_eq!(bad.error.as_ref().unwrap().code, ErrorCode::ActionFailed);
        // The per-check detail still rides along for the harness.
        assert_eq!(bad.data.unwrap()["passed"], false);
    }
}
