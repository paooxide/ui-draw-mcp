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
    flows: Option<crate::flow::FlowStore>,
}

impl BrowserModule {
    pub fn new(backend: Arc<dyn BrowserBackend>) -> Self {
        BrowserModule {
            backend,
            flows: None,
        }
    }

    /// Enable `browser_flow` (save/replay UI tests) backed by a JSON file.
    pub fn with_flow_store(mut self, store: crate::flow::FlowStore) -> Self {
        self.flows = Some(store);
        self
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

    async fn viewport(&self, args: &Value) -> Envelope {
        let target = match require(args, "target_id", "browser_viewport") {
            Ok(t) => t,
            Err(e) => return e,
        };
        let width = args.get("width").and_then(Value::as_u64).unwrap_or(0) as u32;
        let height = args.get("height").and_then(Value::as_u64).unwrap_or(0) as u32;
        let mobile = args.get("mobile").and_then(Value::as_bool).unwrap_or(false);
        let scale = args.get("scale").and_then(Value::as_f64).unwrap_or(1.0);
        match self
            .backend
            .set_viewport(target, width, height, mobile, scale)
            .await
        {
            Ok(v) => Envelope::ok("browser_viewport", v),
            Err(e) => browser_err("browser_viewport", e),
        }
    }

    async fn flow(&self, args: &Value) -> Envelope {
        let tool = "browser_flow";
        let Some(store) = self.flows.as_ref() else {
            return Envelope::fail_with(
                tool,
                ErrorCode::UnsupportedOs,
                "browser_flow is not enabled (no flow store configured)",
                "run agentctl with a state dir so flows can be saved",
            );
        };
        let action = str_arg(args, "action").unwrap_or("list");
        match action {
            "save" => {
                let Some(name) = str_arg(args, "name") else {
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, "save needs 'name'");
                };
                let Some(steps) = args.get("steps").and_then(Value::as_array) else {
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, "save needs 'steps' array");
                };
                match store.save(name, steps.clone(), now_ms()) {
                    Ok(f) => Envelope::ok(tool, json!({ "name": f.name, "steps": f.steps.len() })),
                    Err(e) => flow_err(tool, e),
                }
            }
            "list" => match store.list() {
                Ok(fs) => {
                    let rows: Vec<Value> = fs
                        .iter()
                        .map(|f| json!({ "name": f.name, "steps": f.steps.len() }))
                        .collect();
                    Envelope::ok(tool, json!({ "flows": rows, "count": rows.len() }))
                }
                Err(e) => flow_err(tool, e),
            },
            "get" => {
                let Some(name) = str_arg(args, "name") else {
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, "get needs 'name'");
                };
                match store.get(name) {
                    Ok(Some(f)) => Envelope::ok(tool, json!({ "name": f.name, "steps": f.steps })),
                    Ok(None) => Envelope::fail(tool, ErrorCode::NotFound, format!("no flow '{name}'")),
                    Err(e) => flow_err(tool, e),
                }
            }
            "delete" => {
                let Some(name) = str_arg(args, "name") else {
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, "delete needs 'name'");
                };
                match store.delete(name) {
                    Ok(removed) => Envelope::ok(tool, json!({ "deleted": removed })),
                    Err(e) => flow_err(tool, e),
                }
            }
            "run" => {
                let Some(name) = str_arg(args, "name") else {
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, "run needs 'name'");
                };
                let Some(target) = str_arg(args, "target_id") else {
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, "run needs 'target_id'");
                };
                let flow = match store.get(name) {
                    Ok(Some(f)) => f,
                    Ok(None) => {
                        return Envelope::fail(tool, ErrorCode::NotFound, format!("no flow '{name}'"))
                    }
                    Err(e) => return flow_err(tool, e),
                };
                let cont = args
                    .get("continue_on_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                self.replay(tool, target, &flow, cont).await
            }
            other => Envelope::fail(tool, ErrorCode::InvalidArgs, format!("unknown action '{other}'")),
        }
    }

    /// Replay a flow's steps against `target`, stopping at the first failure
    /// unless `cont`. A green run never invokes a model.
    async fn replay(
        &self,
        tool: &str,
        target: &str,
        flow: &crate::flow::Flow,
        cont: bool,
    ) -> Envelope {
        let mut results = Vec::new();
        let mut passed = true;
        for (i, step) in flow.steps.iter().enumerate() {
            let (ok, detail) = self.run_step(target, step).await;
            results.push(json!({ "i": i, "op": step.get("op"), "ok": ok, "detail": detail }));
            if !ok {
                passed = false;
                if !cont {
                    break;
                }
            }
        }
        let data = json!({
            "name": flow.name, "passed": passed,
            "ran": results.len(), "steps": results,
        });
        if passed {
            Envelope::ok(tool, data)
        } else {
            Envelope {
                ok: false,
                tool: tool.into(),
                data: Some(data),
                error: Some(ToolError {
                    code: ErrorCode::ActionFailed,
                    message: format!("flow '{}' failed", flow.name),
                    suggestion: Some("see data.steps for the failing step".into()),
                }),
                image: None,
            }
        }
    }

    /// Execute one replay step. Returns (ok, detail).
    async fn run_step(&self, target: &str, step: &Value) -> (bool, Value) {
        let op = step.get("op").and_then(Value::as_str).unwrap_or("");
        let r: Result<Value, BrowserError> = match op {
            "navigate" => {
                self.backend
                    .navigate(
                        target,
                        str_arg(step, "action").unwrap_or("goto"),
                        str_arg(step, "url"),
                    )
                    .await
            }
            "act" => {
                let locator = if let Some(r) = str_arg(step, "ref") {
                    crate::backend::Locator::Ref(r)
                } else if let Some(q) = str_arg(step, "query") {
                    crate::backend::Locator::Selector {
                        by: str_arg(step, "by").unwrap_or("css"),
                        query: q,
                    }
                } else {
                    return (false, json!("act step needs 'ref' or 'query'"));
                };
                self.backend
                    .act(
                        target,
                        locator,
                        str_arg(step, "action").unwrap_or("click"),
                        str_arg(step, "value"),
                    )
                    .await
            }
            "viewport" => {
                let width = step.get("width").and_then(Value::as_u64).unwrap_or(0) as u32;
                let height = step.get("height").and_then(Value::as_u64).unwrap_or(0) as u32;
                let mobile = step.get("mobile").and_then(Value::as_bool).unwrap_or(false);
                let scale = step.get("scale").and_then(Value::as_f64).unwrap_or(1.0);
                self.backend
                    .set_viewport(target, width, height, mobile, scale)
                    .await
            }
            "wait" => {
                let (cond, arg) = if let Some(sel) = str_arg(step, "selector") {
                    ("selector", Some(sel))
                } else if step.get("navigation").is_some() {
                    ("navigation", None)
                } else {
                    ("network_idle", None)
                };
                let t = step.get("timeout_ms").and_then(Value::as_u64).unwrap_or(10_000);
                self.backend.wait(target, cond, arg, t).await
            }
            "capture" => {
                self.backend
                    .capture(target, str_arg(step, "action").unwrap_or("start"), step)
                    .await
            }
            "assert" => match self.backend.assert(target, step).await {
                // An assert's own pass/fail is the step's ok.
                Ok(v) => {
                    let passed = v.get("passed").and_then(Value::as_bool) == Some(true);
                    return (passed, v);
                }
                Err(e) => Err(e),
            },
            other => return (false, json!(format!("unknown step op '{other}'"))),
        };
        match r {
            Ok(v) => (true, v),
            Err(e) => (false, json!(browser_err_msg(&e))),
        }
    }
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn flow_err(tool: &str, e: crate::flow::FlowError) -> Envelope {
    match e {
        crate::flow::FlowError::Invalid(m) => Envelope::fail(tool, ErrorCode::InvalidArgs, m),
        crate::flow::FlowError::Io(m) => Envelope::fail(tool, ErrorCode::ActionFailed, m),
    }
}

fn browser_err_msg(e: &BrowserError) -> String {
    match e {
        BrowserError::PermissionDenied(m)
        | BrowserError::NotFound(m)
        | BrowserError::Unsupported(m)
        | BrowserError::Timeout(m)
        | BrowserError::Failed(m) => m.clone(),
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
                "browser_viewport",
                Category::Browser,
                Tier::Standard,
                "Emulate a viewport for responsive testing: override the page's device metrics \
                 (width/height, optionally mobile and a device scale factor). Call with width=0 \
                 (or omitted) to clear the override and restore the real window size.",
                obj(
                    json!({
                        "target_id": { "type": "string" },
                        "width": { "type": "integer", "description": "css px; 0 clears the override" },
                        "height": { "type": "integer", "description": "css px" },
                        "mobile": { "type": "boolean", "description": "emulate a mobile device (touch, meta viewport)" },
                        "scale": { "type": "number", "description": "device scale factor (default 1)" }
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
                 errors when it fails. Functional clauses: text/not_text (in page text), url \
                 (substring), selector (+min_count), no_console_errors and no_failed_requests \
                 (need browser_capture started). UX clauses: a11y (built-in WCAG rules: alt text, \
                 form labels, control names, contrast, target size, positive tabindex, duplicate \
                 ids, page lang), style (design-token conformance: colors/fonts/font_sizes/spacing \
                 allow-lists), component (role/visible/states of one element). 'within' scopes the \
                 UX clauses to a component subtree. Settle first with wait_selector or \
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
                        "within": { "type": "string", "description": "scope a11y/style/component checks to this css root (component testing)" },
                        "a11y": { "description": "true, or {ignore:[rules], contrast:false, target_size:false, contrast_sample:N} to run the built-in accessibility audit" },
                        "style": { "type": "object", "description": "design-token conformance: {colors:[], fonts:[], font_sizes:[], spacing:[]} allow-lists; off-token values fail" },
                        "component": { "type": "object", "description": "{selector, visible, role, states:{disabled,expanded,checked,...}} assertions on one element" },
                        "wait_selector": { "type": "string", "description": "settle: wait for this selector first" },
                        "wait_network_idle": { "type": "boolean", "description": "settle: wait for network idle first" },
                        "timeout_ms": { "type": "integer", "description": "settle timeout (default 8000)" }
                    }),
                    json!(["target_id"]),
                ),
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_flow",
                Category::Browser,
                Tier::Standard,
                "Save and replay a browser UI test. 'save' (name + steps) records a flow; 'run' \
                 (name + target_id) replays it deterministically, stopping at the first failing \
                 step (set continue_on_error to run all); 'list'/'get'/'delete' manage them. A \
                 step is {op: navigate|act|wait|capture|assert, ...} using the same fields as \
                 those tools (e.g. {op:'act',by:'text',query:'Login',action:'click'}, \
                 {op:'assert',text:'Welcome'}). A green run never needs a model.",
                obj(
                    json!({
                        "action": { "type": "string", "enum": ["save", "run", "list", "get", "delete"] },
                        "name": { "type": "string" },
                        "target_id": { "type": "string", "description": "run: the tab to replay against" },
                        "steps": { "type": "array", "items": { "type": "object" }, "description": "save: the ordered steps" },
                        "continue_on_error": { "type": "boolean", "description": "run: keep going past a failed step" }
                    }),
                    json!(["action"]),
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
            "browser_viewport" => self.viewport(&args).await,
            "browser_eval" => self.eval(&args).await,
            "browser_dialog" => self.dialog(&args).await,
            "browser_network" => self.network(&args).await,
            "browser_cookies" => self.cookies(&args).await,
            "browser_capture" => self.capture(&args).await,
            "browser_assert" => self.assert(&args).await,
            "browser_flow" => self.flow(&args).await,
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
        viewports: Mutex<Vec<String>>,
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
        async fn navigate(&self, _t: &str, a: &str, _u: Option<&str>) -> Result<Value, BrowserError> {
            Ok(json!({ "ok": true, "action": a }))
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
        async fn wait(&self, _t: &str, c: &str, _a: Option<&str>, _ms: u64) -> Result<Value, BrowserError> {
            Ok(json!({ "settled": true, "condition": c }))
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
        async fn set_viewport(&self, _t: &str, w: u32, h: u32, m: bool, s: f64) -> Result<Value, BrowserError> {
            self.viewports.lock().unwrap().push(format!("{w}x{h} mobile={m} scale={s}"));
            Ok(json!({ "width": w, "height": h, "mobile": m }))
        }
    }

    fn module() -> (BrowserModule, Arc<Recorder>) {
        let rec = Arc::new(Recorder::default());
        (BrowserModule::new(rec.clone()), rec)
    }

    fn module_with_flows(tag: &str) -> BrowserModule {
        use crate::flow::FlowStore;
        let mut p = std::env::temp_dir();
        p.push(format!("agentctl-flowtool-{tag}-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&p);
        BrowserModule::new(Arc::new(Recorder::default()))
            .with_flow_store(FlowStore::new(p, 50, 50))
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
    async fn viewport_passes_dimensions_through_and_a_flow_step_reaches_the_backend() {
        let (m, rec) = module();
        assert!(m.viewport(&json!({"target_id":"T","width":390,"height":844,"mobile":true})).await.ok);
        // A viewport step in a replayed flow reaches the same backend call.
        let (ok, _) = m.run_step("T", &json!({"op":"viewport","width":1280,"height":800})).await;
        assert!(ok);
        let v = rec.viewports.lock().unwrap();
        assert_eq!(v[0], "390x844 mobile=true scale=1");
        assert_eq!(v[1], "1280x800 mobile=false scale=1");
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

    #[tokio::test]
    async fn a_saved_flow_replays_green_and_reports_each_step() {
        let m = module_with_flows("green");
        let save = m
            .flow(&json!({"action":"save","name":"login","steps":[
                {"op":"navigate","url":"https://x"},
                {"op":"wait","network_idle":true},
                {"op":"assert","target_id":"ignored","_pass":true}
            ]}))
            .await;
        assert!(save.ok, "{save:?}");
        let run = m.flow(&json!({"action":"run","name":"login","target_id":"T"})).await;
        assert!(run.ok, "green flow should pass: {run:?}");
        let d = run.data.unwrap();
        assert_eq!(d["passed"], true);
        assert_eq!(d["ran"], 3);
    }

    #[tokio::test]
    async fn a_failing_step_stops_the_run_and_marks_it_failed() {
        let m = module_with_flows("red");
        m.flow(&json!({"action":"save","name":"f","steps":[
            {"op":"navigate","url":"https://x"},
            {"op":"assert","_pass":false},
            {"op":"navigate","url":"https://never-reached"}
        ]}))
        .await;
        let run = m.flow(&json!({"action":"run","name":"f","target_id":"T"})).await;
        assert!(!run.ok, "a failing flow is an error");
        let d = run.data.unwrap();
        assert_eq!(d["passed"], false);
        assert_eq!(d["ran"], 2, "stops at the failing assert, third step not reached");
    }

    #[tokio::test]
    async fn run_without_a_target_is_an_invalid_argument() {
        let m = module_with_flows("notgt");
        m.flow(&json!({"action":"save","name":"f","steps":[{"op":"navigate"}]})).await;
        let e = m.flow(&json!({"action":"run","name":"f"})).await;
        assert!(!e.ok);
        assert_eq!(e.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn flow_is_disabled_without_a_store() {
        let (m, _) = module();
        let e = m.flow(&json!({"action":"list"})).await;
        assert!(!e.ok, "no store configured means the tool is unavailable");
    }
}
