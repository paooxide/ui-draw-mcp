use std::collections::BTreeMap;
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
    baselines: Option<crate::visual::VisualStore>,
    profiles: Option<crate::profile::ProfileStore>,
    judge: Option<Arc<mcp_judge::Judge>>,
    showcase: std::sync::Mutex<crate::showcase::ShowcaseConfig>,
}

impl BrowserModule {
    pub fn new(backend: Arc<dyn BrowserBackend>) -> Self {
        BrowserModule {
            backend,
            flows: None,
            baselines: None,
            profiles: None,
            judge: None,
            showcase: std::sync::Mutex::new(crate::showcase::ShowcaseConfig::default()),
        }
    }

    /// Enable `browser_flow` (save/replay UI tests) backed by a JSON file.
    pub fn with_flow_store(mut self, store: crate::flow::FlowStore) -> Self {
        self.flows = Some(store);
        self
    }

    /// Enable the `visual` assert clause (baseline screenshot + pixel diff).
    pub fn with_visual_store(mut self, store: crate::visual::VisualStore) -> Self {
        self.baselines = Some(store);
        self
    }

    /// Enable `browser_profile` (save/restore session states) backed by a JSON file.
    pub fn with_profile_store(mut self, store: crate::profile::ProfileStore) -> Self {
        self.profiles = Some(store);
        self
    }

    /// Enable the `ux` assert clause (judge-scored heuristic review; advisory).
    pub fn with_judge(mut self, judge: Arc<mcp_judge::Judge>) -> Self {
        self.judge = Some(judge);
        self
    }

    /// Enable showcase mode (animated SVG pointer, gliding, ripples, typing HUD).
    pub fn with_showcase(self, config: crate::showcase::ShowcaseConfig) -> Self {
        *self
            .showcase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = config;
        self
    }
}

/// Assert clauses evaluated in the page by the backend. `visual` and `ux` are
/// handled at the module layer instead (they need the baseline store / judge).
const JS_ASSERT_KEYS: &[&str] = &[
    "text",
    "not_text",
    "url",
    "selector",
    "no_console_errors",
    "no_failed_requests",
    "a11y",
    "style",
    "component",
];

/// A per-dimension UX instruction for the judge. Known dimensions get a focused
/// prompt; an unknown one gets a sensible generic prompt so callers can add
/// their own without a code change.
fn ux_instruction(dim: &str) -> String {
    match dim {
        "clarity" => {
            "The screen's purpose and its primary action are immediately clear to a first-time user."
        }
        "hierarchy" => {
            "The visual hierarchy guides the eye: the most important element stands out and the \
             grouping of related content is logical."
        }
        "affordance" => {
            "Interactive elements clearly look interactive and their labels say what they will do."
        }
        "consistency" => {
            "Labels, terminology and controls are consistent with each other and with common \
             platform conventions."
        }
        other => return format!("From a UX standpoint, the screen exhibits good {other}."),
    }
    .to_string()
}

/// Gather the page facts the UX judge reasons over: title, url, headings,
/// action labels, field names and a bounded slice of visible text.
const UX_FACTS_JS: &str = r#"(function(){
  function txt(el){ return ((el&&el.innerText)||'').trim().replace(/\s+/g,' '); }
  var main=document.querySelector('main')||document.body;
  var h=[].slice.call(main.querySelectorAll('h1,h2,h3')).map(txt).filter(Boolean).slice(0,20);
  var a=[].slice.call(main.querySelectorAll('button,a[href],[role=button]')).map(function(b){return (b.getAttribute('aria-label')||txt(b));}).filter(Boolean).slice(0,30);
  var f=[].slice.call(main.querySelectorAll('input,select,textarea')).map(function(i){return i.getAttribute('placeholder')||i.getAttribute('name')||i.getAttribute('type')||'field';}).slice(0,30);
  return { title: document.title, url: location.href, headings: h, actions: a, fields: f, text: txt(main).slice(0,2000) };
})()"#;

/// Decode two base64 PNGs, compare them pixel-for-pixel on a canvas, and return
/// the changed-pixel ratio plus a bounding box. `__BASE__`/`__CUR__` are
/// replaced with base64 (the base64 alphabet has no quotes, so single-quoting
/// is safe). A small per-pixel threshold ignores antialiasing noise.
const VISUAL_DIFF_JS: &str = r#"(async function(){
  var A='__BASE__', B='__CUR__';
  function load(src){ return new Promise(function(res,rej){ var im=new Image(); im.onload=function(){res(im);}; im.onerror=function(){rej(new Error('decode failed'));}; im.src='data:image/png;base64,'+src; }); }
  var ia, ib;
  try{ ia=await load(A); ib=await load(B); }catch(e){ return {error:String(e&&e.message||e)}; }
  if(ia.width!==ib.width||ia.height!==ib.height){ return {dims_match:false, base:ia.width+'x'+ia.height, cur:ib.width+'x'+ib.height, diff_ratio:1}; }
  var w=ia.width, h=ia.height;
  function ctx(){ try{ var c=new OffscreenCanvas(w,h); return c.getContext('2d',{willReadFrequently:true}); }catch(e){ var cv=document.createElement('canvas'); cv.width=w; cv.height=h; return cv.getContext('2d',{willReadFrequently:true}); } }
  var xa=ctx(), xb=ctx();
  xa.drawImage(ia,0,0); xb.drawImage(ib,0,0);
  var da=xa.getImageData(0,0,w,h).data, db=xb.getImageData(0,0,w,h).data;
  var thr=16, changed=0, minx=w, miny=h, maxx=-1, maxy=-1;
  for(var i=0;i<da.length;i+=4){ var d=Math.abs(da[i]-db[i])+Math.abs(da[i+1]-db[i+1])+Math.abs(da[i+2]-db[i+2])+Math.abs(da[i+3]-db[i+3]); if(d>thr){ changed++; var p=i/4, px=p%w, py=(p/w)|0; if(px<minx)minx=px; if(px>maxx)maxx=px; if(py<miny)miny=py; if(py>maxy)maxy=py; } }
  var total=w*h;
  return { dims_match:true, w:w, h:h, changed:changed, total:total, diff_ratio: total? changed/total : 0, bbox: (maxx>=0)? {x:minx,y:miny,w:maxx-minx+1,h:maxy-miny+1} : null };
})()"#;

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
        let profile_name = launch
            .as_ref()
            .and_then(|l| l.get("profile"))
            .and_then(Value::as_str)
            .or_else(|| str_arg(args, "profile"))
            .map(|s| s.to_string());

        // Resolve the profile *before* connecting: an unknown name must fail
        // without launching (and leaking) a browser, and it must fail loudly
        // rather than connect without the session the caller asked for.
        let profile = match profile_name {
            None => None,
            Some(ref pname) => {
                let Some(ref store) = self.profiles else {
                    return Envelope::fail(
                        "browser_connect",
                        ErrorCode::InvalidArgs,
                        format!(
                            "profile '{pname}' requested but browser profiles are not enabled \
                             (no profile store configured)"
                        ),
                    );
                };
                match store.get(pname) {
                    Ok(Some(p)) => Some(p),
                    Ok(None) => {
                        return Envelope::fail(
                            "browser_connect",
                            ErrorCode::NotFound,
                            format!("profile '{pname}' not found"),
                        )
                    }
                    Err(crate::profile::ProfileError::Io(m)) => {
                        return Envelope::fail("browser_connect", ErrorCode::ActionFailed, m)
                    }
                    Err(crate::profile::ProfileError::Invalid(m)) => {
                        return Envelope::fail("browser_connect", ErrorCode::InvalidArgs, m)
                    }
                }
            }
        };

        let res = self.backend.connect(attach_port, launch).await;
        match res {
            Ok(mut val) => {
                if let Some(prof) = profile {
                    let browser_id =
                        val.get("browser_id").and_then(Value::as_u64).unwrap_or(1) as u32;
                    let outcome = self.restore_profile_onto_first_tab(browser_id, &prof).await;
                    if let Some(map) = val.as_object_mut() {
                        match outcome {
                            Ok(()) => {
                                map.insert("profile_restored".into(), json!(prof.name));
                            }
                            Err(msg) => {
                                // The browser is connected, so this is not a
                                // failed call; but it must not claim success.
                                map.insert("profile_restored".into(), json!(false));
                                map.insert("profile_restore_error".into(), json!(msg));
                            }
                        }
                    }
                }
                Envelope::ok("browser_connect", val)
            }
            Err(e) => browser_err("browser_connect", e),
        }
    }

    /// Put a saved profile onto the first tab of a freshly connected browser.
    ///
    /// Order matters: cookies and web storage are scoped to an origin, and a
    /// new tab is `about:blank`, where storage writes throw. So go to the
    /// profile's saved URL first, wait for that document to finish loading,
    /// and only then restore. Returns the reason on any failure.
    async fn restore_profile_onto_first_tab(
        &self,
        browser_id: u32,
        prof: &crate::profile::Profile,
    ) -> Result<(), String> {
        let tabs = self
            .backend
            .tabs(browser_id, "list", None, None)
            .await
            .map_err(|e| format!("could not list tabs: {}", browser_err_msg(&e)))?;
        let first = tabs
            .get("tabs")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .ok_or_else(|| "browser has no tab to restore the profile onto".to_string())?;
        let target_id = first
            .get("target_id")
            .and_then(Value::as_str)
            .ok_or_else(|| "first tab has no target_id".to_string())?;
        let before = first.get("url").and_then(Value::as_str).unwrap_or("");

        let dest = prof
            .url
            .as_deref()
            .filter(|u| !u.is_empty() && *u != "about:blank");
        if let Some(dest) = dest {
            self.backend
                .navigate(target_id, "goto", Some(dest))
                .await
                .map_err(|e| format!("could not open {dest}: {}", browser_err_msg(&e)))?;
            self.wait_page_loaded(target_id, before, dest).await?;
        }

        let state = json!({
            "cookies": prof.cookies,
            "localStorage": prof.local_storage,
            "sessionStorage": prof.session_storage,
        });
        let res = self
            .backend
            .profile_restore(target_id, &state)
            .await
            .map_err(|e| format!("restore failed: {}", browser_err_msg(&e)))?;
        if res.get("ok").and_then(Value::as_bool) == Some(false) {
            let why = res
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("storage restore failed");
            let hint = if dest.is_none() {
                " (the profile has no saved URL, so web storage has no page to live on)"
            } else {
                ""
            };
            return Err(format!("restore failed: {why}{hint}"));
        }
        Ok(())
    }

    /// Block until `target` shows a finished document other than `before`
    /// (the pre-navigation URL), or is already at `dest`. Polls the page: the
    /// old document still reports `complete` for a moment after navigating.
    async fn wait_page_loaded(&self, target: &str, before: &str, dest: &str) -> Result<(), String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            if let Ok(v) = self
                .backend
                .eval(target, "location.href + '|' + document.readyState")
                .await
            {
                let probe = v.get("result").and_then(Value::as_str).unwrap_or("");
                if let Some((href, state)) = probe.rsplit_once('|') {
                    let moved = href != before
                        || href == dest
                        || before.trim_end_matches('/') == dest.trim_end_matches('/');
                    if state == "complete" && moved && href != "about:blank" {
                        return Ok(());
                    }
                }
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!("{dest} did not finish loading within 15s"));
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
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
                within: str_arg(args, "within"),
                text: str_arg(args, "text"),
                index: args
                    .get("index")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize),
            }
        } else {
            return Envelope::fail_with(
                "browser_act",
                ErrorCode::InvalidArgs,
                "need 'ref' (from browser_query/snapshot) or 'query' (with optional 'by', 'within', 'text', 'index')",
                "pass ref, or query plus by=css|xpath|text",
            );
        };
        let action = str_arg(args, "action").unwrap_or("click");
        let secret = args.get("secret").and_then(Value::as_bool) == Some(true);
        result(
            "browser_act",
            self.backend
                .act_masked(target, locator, action, str_arg(args, "value"), secret)
                .await,
        )
    }

    async fn fill_form(&self, args: &Value) -> Envelope {
        let tool = "browser_fill_form";
        let target = match require(args, "target_id", tool) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let Some(fields) = args.get("fields").and_then(Value::as_array) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'fields' array");
        };
        if fields.is_empty() {
            return Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                "'fields' array must not be empty",
            );
        }
        let submit = args.get("submit");
        result(
            tool,
            self.backend
                .fill_form(target, &Value::Array(fields.clone()), submit)
                .await,
        )
    }

    async fn extract(&self, args: &Value) -> Envelope {
        let tool = "browser_extract";
        let target = match require(args, "target_id", tool) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let Some(schema) = args.get("schema") else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'schema' object");
        };
        let within = str_arg(args, "within");
        result(tool, self.backend.extract(target, schema, within).await)
    }

    async fn profile(&self, args: &Value) -> Envelope {
        let tool = "browser_profile";
        let Some(store) = self.profiles.as_ref() else {
            return Envelope::fail_with(
                tool,
                ErrorCode::UnsupportedOs,
                "browser_profile is not enabled (no profile store configured)",
                "run agentctl with a state dir so profiles can be saved",
            );
        };
        let action = str_arg(args, "action").unwrap_or("list");
        match action {
            "save" => {
                let target = match require(args, "target_id", tool) {
                    Ok(t) => t,
                    Err(e) => return e,
                };
                let Some(name) = str_arg(args, "name") else {
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, "save needs 'name'");
                };
                let state = match self.backend.profile_state(target).await {
                    Ok(s) => s,
                    Err(e) => return browser_err(tool, e),
                };
                let cookies = state
                    .get("cookies")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let ls = state.get("localStorage").cloned().unwrap_or(json!({}));
                let ss = state.get("sessionStorage").cloned().unwrap_or(json!({}));
                let url = state
                    .get("url")
                    .and_then(Value::as_str)
                    .map(|s| s.to_string());
                match store.save(name, cookies, ls, ss, url, now_ms()) {
                    Ok(p) => Envelope::ok(
                        tool,
                        json!({
                            "saved": true,
                            "name": p.name,
                            "cookies_count": p.cookies.len(),
                            "local_storage_keys": p.local_storage.as_object().map(|o| o.len()).unwrap_or(0),
                            "session_storage_keys": p.session_storage.as_object().map(|o| o.len()).unwrap_or(0),
                            "url": p.url,
                        }),
                    ),
                    Err(crate::profile::ProfileError::Invalid(m)) => {
                        Envelope::fail(tool, ErrorCode::InvalidArgs, m)
                    }
                    Err(crate::profile::ProfileError::Io(m)) => {
                        Envelope::fail(tool, ErrorCode::ActionFailed, m)
                    }
                }
            }
            "restore" => {
                let target = match require(args, "target_id", tool) {
                    Ok(t) => t,
                    Err(e) => return e,
                };
                let Some(name) = str_arg(args, "name") else {
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, "restore needs 'name'");
                };
                let profile = match store.get(name) {
                    Ok(Some(p)) => p,
                    Ok(None) => {
                        return Envelope::fail(
                            tool,
                            ErrorCode::NotFound,
                            format!("profile '{name}' not found"),
                        )
                    }
                    Err(crate::profile::ProfileError::Io(m)) => {
                        return Envelope::fail(tool, ErrorCode::ActionFailed, m)
                    }
                    Err(crate::profile::ProfileError::Invalid(m)) => {
                        return Envelope::fail(tool, ErrorCode::InvalidArgs, m)
                    }
                };
                let state = json!({
                    "cookies": profile.cookies,
                    "localStorage": profile.local_storage,
                    "sessionStorage": profile.session_storage,
                });
                match self.backend.profile_restore(target, &state).await {
                    Ok(res) if res.get("ok").and_then(Value::as_bool) == Some(false) => {
                        // The backend reports a failed storage write inside an
                        // otherwise successful reply; do not call that restored.
                        Envelope::fail(
                            tool,
                            ErrorCode::ActionFailed,
                            format!(
                                "restore of '{name}' failed: {}",
                                res.get("error")
                                    .and_then(Value::as_str)
                                    .unwrap_or("storage restore failed")
                            ),
                        )
                    }
                    Ok(mut res) => {
                        if let Some(map) = res.as_object_mut() {
                            map.insert("restored".into(), json!(true));
                            map.insert("name".into(), json!(name));
                            Envelope::ok(tool, res)
                        } else {
                            Envelope::ok(
                                tool,
                                json!({ "ok": true, "name": name, "restored": true, "detail": res }),
                            )
                        }
                    }
                    Err(e) => browser_err(tool, e),
                }
            }
            "list" => match store.list() {
                Ok(ps) => {
                    let rows: Vec<Value> = ps
                        .iter()
                        .map(|p| {
                            json!({
                                "name": p.name,
                                "cookies_count": p.cookies_count,
                                "local_storage_count": p.local_storage_count,
                                "url": p.url,
                                "updated_ms": p.updated_ms,
                            })
                        })
                        .collect();
                    Envelope::ok(tool, json!({ "profiles": rows, "count": rows.len() }))
                }
                Err(crate::profile::ProfileError::Io(m)) => {
                    Envelope::fail(tool, ErrorCode::ActionFailed, m)
                }
                Err(crate::profile::ProfileError::Invalid(m)) => {
                    Envelope::fail(tool, ErrorCode::InvalidArgs, m)
                }
            },
            "delete" => {
                let Some(name) = str_arg(args, "name") else {
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, "delete needs 'name'");
                };
                match store.delete(name) {
                    Ok(true) => Envelope::ok(tool, json!({ "deleted": true, "name": name })),
                    Ok(false) => Envelope::fail(
                        tool,
                        ErrorCode::NotFound,
                        format!("profile '{name}' not found"),
                    ),
                    Err(crate::profile::ProfileError::Io(m)) => {
                        Envelope::fail(tool, ErrorCode::ActionFailed, m)
                    }
                    Err(crate::profile::ProfileError::Invalid(m)) => {
                        Envelope::fail(tool, ErrorCode::InvalidArgs, m)
                    }
                }
            }
            other => Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                format!("unknown action '{other}' (use save|restore|list|delete)"),
            ),
        }
    }

    async fn branch(&self, args: &Value) -> Envelope {
        let tool = "browser_branch";
        let action = str_arg(args, "action").unwrap_or("list");
        match action {
            "create" => {
                let target = match require(args, "target_id", tool) {
                    Ok(t) => t,
                    Err(e) => return e,
                };
                let branch_id = match require(args, "branch_id", tool) {
                    Ok(b) => b,
                    Err(e) => return e,
                };
                result(tool, self.backend.branch_create(target, branch_id).await)
            }
            "commit" => {
                let branch_id = match require(args, "branch_id", tool) {
                    Ok(b) => b,
                    Err(e) => return e,
                };
                result(tool, self.backend.branch_commit(branch_id).await)
            }
            "discard" => {
                let branch_id = match require(args, "branch_id", tool) {
                    Ok(b) => b,
                    Err(e) => return e,
                };
                result(tool, self.backend.branch_discard(branch_id).await)
            }
            "switch" => {
                let branch_id = match require(args, "branch_id", tool) {
                    Ok(b) => b,
                    Err(e) => return e,
                };
                result(tool, self.backend.branch_switch(branch_id).await)
            }
            "list" => {
                let target = str_arg(args, "target_id");
                result(tool, self.backend.branch_list(target).await)
            }
            other => Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                format!(
                    "unknown branch action '{other}'; use create, commit, discard, switch, or list"
                ),
            ),
        }
    }

    async fn checkpoint(&self, args: &Value) -> Envelope {
        let tool = "browser_checkpoint";
        let action = str_arg(args, "action").unwrap_or("save");
        match action {
            "save" => {
                let target = match require(args, "target_id", tool) {
                    Ok(t) => t,
                    Err(e) => return e,
                };
                let tag = str_arg(args, "tag");
                result(tool, self.backend.checkpoint_save(target, tag).await)
            }
            "rollback" => {
                let target = match require(args, "target_id", tool) {
                    Ok(t) => t,
                    Err(e) => return e,
                };
                let tag = str_arg(args, "tag");
                result(tool, self.backend.checkpoint_rollback(target, tag).await)
            }
            "list" => {
                let target = str_arg(args, "target_id");
                result(tool, self.backend.checkpoint_list(target).await)
            }
            "delete" => {
                let target = match require(args, "target_id", tool) {
                    Ok(t) => t,
                    Err(e) => return e,
                };
                let tag = str_arg(args, "tag");
                result(tool, self.backend.checkpoint_delete(target, tag).await)
            }
            other => Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                format!("unknown checkpoint action '{other}'; use save, rollback, list, or delete"),
            ),
        }
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
        // exactly one of selector | dom_settled | navigation | network_idle
        let (cond, arg) = if let Some(sel) = str_arg(args, "selector") {
            ("selector", Some(sel))
        } else if args.get("dom_settled").and_then(Value::as_bool) == Some(true)
            || str_arg(args, "condition") == Some("dom_settled")
        {
            ("dom_settled", None)
        } else if args.get("htmx_settled").and_then(Value::as_bool) == Some(true)
            || str_arg(args, "condition") == Some("htmx_settled")
        {
            ("htmx_settled", None)
        } else if args.get("navigation").is_some()
            || str_arg(args, "condition") == Some("navigation")
        {
            ("navigation", None)
        } else if args.get("network_idle").and_then(Value::as_bool) == Some(true)
            || str_arg(args, "condition") == Some("network_idle")
        {
            ("network_idle", None)
        } else if args.get("challenge_cleared").and_then(Value::as_bool) == Some(true)
            || str_arg(args, "condition") == Some("challenge_cleared")
            || str_arg(args, "condition") == Some("challenge")
        {
            ("challenge_cleared", None)
        } else {
            return Envelope::fail(
                "browser_wait",
                ErrorCode::InvalidArgs,
                "provide one of 'selector', 'dom_settled', 'htmx_settled', 'navigation', 'network_idle', or 'challenge_cleared'",
            );
        };
        let nav_window = match args.get("navigation_timeout_ms") {
            None | Some(Value::Null) => None,
            Some(v) => match nav_window_arg(v) {
                Some(n) => Some(n),
                None => {
                    return Envelope::fail(
                        "browser_wait",
                        ErrorCode::InvalidArgs,
                        format!(
                            "navigation_timeout_ms must be an integer from 0 to {}",
                            crate::backend::NAV_EXPECT_MAX_MS
                        ),
                    )
                }
            },
        };
        if nav_window.is_some() && cond != "navigation" {
            return Envelope::fail(
                "browser_wait",
                ErrorCode::InvalidArgs,
                "navigation_timeout_ms only applies to the 'navigation' condition",
            );
        }
        result(
            "browser_wait",
            self.backend
                .wait_window(target, cond, arg, timeout_ms, nav_window)
                .await,
        )
    }

    async fn challenge(&self, args: &Value) -> Envelope {
        let tool = "browser_challenge";
        let target = match require(args, "target_id", tool) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let action = str_arg(args, "action").unwrap_or("detect");
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(30_000);

        match action {
            "detect" | "status" => {
                match crate::challenge::ChallengeManager::detect(self.backend.as_ref(), target)
                    .await
                {
                    Ok(st) => Envelope::ok(tool, json!(st)),
                    Err(e) => browser_err(tool, e),
                }
            }
            "wait" | "clear" => {
                match crate::challenge::ChallengeManager::wait_for_clearance(
                    self.backend.as_ref(),
                    target,
                    timeout_ms,
                )
                .await
                {
                    Ok(v) => Envelope::ok(tool, v),
                    Err(e) => browser_err(tool, e),
                }
            }
            "hud_show" => {
                let kind_str = str_arg(args, "kind").unwrap_or("unknown");
                let kind = crate::challenge::ChallengeKind::from_name(kind_str);
                match crate::challenge::ChallengeManager::inject_hud(
                    self.backend.as_ref(),
                    target,
                    &kind,
                )
                .await
                {
                    Ok(()) => Envelope::ok(tool, json!({ "hud": "visible", "kind": kind_str })),
                    Err(e) => browser_err(tool, e),
                }
            }
            "hud_hide" => {
                match crate::challenge::ChallengeManager::remove_hud(self.backend.as_ref(), target)
                    .await
                {
                    Ok(()) => Envelope::ok(tool, json!({ "hud": "removed" })),
                    Err(e) => browser_err(tool, e),
                }
            }
            other => Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                format!(
                    "unknown challenge action '{other}'; use detect, wait, hud_show, or hud_hide"
                ),
            ),
        }
    }

    async fn record(&self, args: &Value) -> Envelope {
        let tool = "browser_record";
        let target = match require(args, "target_id", tool) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let action = str_arg(args, "action").unwrap_or("status");
        match action {
            "start" => {
                let dialogs = match str_arg(args, "dialogs") {
                    None => None,
                    Some(s) => match crate::cdp::RecordDialogs::parse(s) {
                        Some(d) => Some(d),
                        None => {
                            return Envelope::fail_with(
                                tool,
                                ErrorCode::InvalidArgs,
                                format!("unknown dialogs setting '{s}'"),
                                "use 'human' (the person answers), 'accept' or 'dismiss'",
                            )
                        }
                    },
                };
                match crate::record::RecordManager::start_with(
                    self.backend.as_ref(),
                    target,
                    dialogs,
                )
                .await
                {
                    Ok(v) => Envelope::ok(tool, v),
                    Err(e) => browser_err(tool, e),
                }
            }
            "status" => {
                match crate::record::RecordManager::status(self.backend.as_ref(), target).await {
                    Ok(v) => Envelope::ok(tool, v),
                    Err(e) => browser_err(tool, e),
                }
            }
            "stop" => {
                if let Some(name) = str_arg(args, "name") {
                    let Some(store) = self.flows.as_ref() else {
                        return Envelope::fail_with(
                            tool,
                            ErrorCode::UnsupportedOs,
                            "browser_record auto-save is not enabled (no flow store configured)",
                            "run agentctl with a state dir so flows can be saved",
                        );
                    };
                    match crate::record::RecordManager::stop_and_save(
                        self.backend.as_ref(),
                        store,
                        target,
                        name,
                    )
                    .await
                    {
                        Ok(f) => Envelope::ok(
                            tool,
                            json!({ "saved": true, "name": f.name, "steps": f.steps.len(), "flow": f }),
                        ),
                        Err(e) => flow_err(tool, e),
                    }
                } else {
                    match crate::record::RecordManager::stop(self.backend.as_ref(), target).await {
                        Ok(steps) => Envelope::ok(
                            tool,
                            json!({ "recording": false, "steps": steps, "count": steps.len() }),
                        ),
                        Err(e) => browser_err(tool, e),
                    }
                }
            }
            other => Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                format!("unknown record action '{other}'; use start, stop, or status"),
            ),
        }
    }

    async fn showcase(&self, args: &Value) -> Envelope {
        let tool = "browser_showcase";
        let target = match require(args, "target_id", tool) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let mut cfg = self
            .showcase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(en) = args.get("enabled").and_then(Value::as_bool) {
            cfg.enabled = en;
            if en && matches!(cfg.speed, crate::showcase::ShowcaseSpeed::Off) {
                cfg.speed = crate::showcase::ShowcaseSpeed::Demo;
            }
        }
        if let Some(spd) = str_arg(args, "speed") {
            if let Some(s) = crate::showcase::ShowcaseSpeed::parse(spd) {
                cfg.speed = s;
                cfg.enabled = !matches!(s, crate::showcase::ShowcaseSpeed::Off);
            } else {
                return Envelope::fail(
                    tool,
                    ErrorCode::InvalidArgs,
                    "speed must be cinematic, demo, snappy, or off",
                );
            }
        }
        if let Some(r) = args.get("click_ripple").and_then(Value::as_bool) {
            cfg.click_ripple = r;
        }
        if let Some(h) = args.get("typing_hud").and_then(Value::as_bool) {
            cfg.typing_hud = h;
        }
        if let Some(style) = str_arg(args, "cursor_style") {
            if let Some(cs) = crate::showcase::CursorStyle::parse(style) {
                cfg.cursor_style = cs;
            }
        }
        if let Some(dur) = args.get("glide_ms").and_then(Value::as_u64) {
            cfg.custom_glide_ms = Some(dur);
        }
        *self
            .showcase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = cfg.clone();
        result(tool, self.backend.showcase(target, Some(cfg)).await)
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
        let opts = crate::backend::EvalOptions {
            timeout_ms: args.get("timeout_ms").and_then(Value::as_u64),
            detached: args
                .get("detached")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        };
        result(
            "browser_eval",
            self.backend.eval_with(target, expr, &opts).await,
        )
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
        match self.assert_full(target, args).await {
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

    /// The whole assertion: the in-page JS clauses (via the backend), plus the
    /// `visual` diff and `ux` review, which need this layer's baseline store and
    /// judge. Returns the same `{passed, checks}` shape as the backend, so the
    /// flow engine and `agentctl test` report treat every clause alike.
    async fn assert_full(&self, target: &str, spec: &Value) -> Result<Value, BrowserError> {
        let mut checks: Vec<Value> = Vec::new();
        let wants_visual = spec.get("visual").is_some();
        let wants_ux = spec.get("ux").is_some();
        let wants_js = JS_ASSERT_KEYS.iter().any(|k| spec.get(*k).is_some());
        // Run the in-page clauses when the spec asks for one, or when it asks
        // for nothing here at all (so a plain functional assert still reaches
        // the backend); skip only when the spec is purely visual/ux.
        if wants_js || (!wants_visual && !wants_ux) {
            let r = self.backend.assert(target, spec).await?;
            if let Some(arr) = r.get("checks").and_then(Value::as_array) {
                checks.extend(arr.iter().cloned());
            }
        }
        if spec.get("visual").is_some() {
            checks.push(self.visual_check(target, spec).await);
        }
        if spec.get("ux").is_some() {
            checks.push(self.ux_check(target, spec).await);
        }
        if checks.is_empty() {
            return Err(BrowserError::Failed(
                "no assertions given (text/not_text/url/selector/no_console_errors/\
                 no_failed_requests/a11y/style/component/visual/ux)"
                    .into(),
            ));
        }
        let passed = checks
            .iter()
            .all(|c| c.get("ok").and_then(Value::as_bool) == Some(true));
        Ok(json!({ "passed": passed, "checks": checks }))
    }

    /// Visual regression: screenshot now, compare to a stored baseline. The
    /// first run for a name saves the baseline and passes; later runs pass when
    /// the changed-pixel ratio is within tolerance and the dimensions match.
    /// Any internal failure becomes a failing check rather than aborting the
    /// whole assertion, so it reports cleanly.
    async fn visual_check(&self, target: &str, spec: &Value) -> Value {
        let fail = |d: String| json!({ "name": "visual", "ok": false, "detail": d });
        let v = &spec["visual"];
        let (name, tolerance, node_ref) = match v {
            Value::String(s) => (s.clone(), 0.01_f64, None),
            _ => (
                v.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                v.get("tolerance").and_then(Value::as_f64).unwrap_or(0.01),
                v.get("ref").and_then(Value::as_str).map(String::from),
            ),
        };
        if name.trim().is_empty() {
            return fail("visual needs a baseline name".into());
        }
        let Some(store) = self.baselines.as_ref() else {
            return fail("visual store not configured (run with a state dir)".into());
        };
        let shot = match self.backend.screenshot(target, node_ref.as_deref()).await {
            Ok(s) => s,
            Err(e) => return fail(format!("screenshot failed: {}", browser_err_msg(&e))),
        };
        let base = match store.get(&name) {
            Ok(b) => b,
            Err(e) => return fail(format!("baseline load failed: {e:?}")),
        };
        let Some(base) = base else {
            return match store.save(&name, shot.base64, shot.width, shot.height, now_ms()) {
                Ok(_) => json!({ "name": "visual", "ok": true, "created": true,
                    "detail": format!("baseline created: {name}") }),
                Err(e) => fail(format!("baseline save failed: {e:?}")),
            };
        };
        let diff = match self
            .visual_diff(target, &base.png_base64, &shot.base64)
            .await
        {
            Ok(d) => d,
            Err(e) => return fail(format!("diff failed: {}", browser_err_msg(&e))),
        };
        if let Some(err) = diff.get("error").and_then(Value::as_str) {
            return fail(format!("diff error: {err}"));
        }
        if diff.get("dims_match").and_then(Value::as_bool) != Some(true) {
            let b = diff.get("base").and_then(Value::as_str).unwrap_or("?");
            let c = diff.get("cur").and_then(Value::as_str).unwrap_or("?");
            return json!({ "name": "visual", "ok": false,
                "detail": format!("dimensions changed {b} -> {c}"), "diff": diff });
        }
        let ratio = diff
            .get("diff_ratio")
            .and_then(Value::as_f64)
            .unwrap_or(1.0);
        json!({
            "name": "visual",
            "ok": ratio <= tolerance,
            "detail": format!("{:.2}% changed (tol {:.2}%)", ratio * 100.0, tolerance * 100.0),
            "diff": diff,
        })
    }

    /// Diff two base64 PNGs in the page: decode both, compare pixels on a canvas,
    /// return the changed ratio and a bounding box. Done in-page to avoid an
    /// image-decoding dependency and to keep the whole comparison in one place.
    async fn visual_diff(
        &self,
        target: &str,
        base_b64: &str,
        cur_b64: &str,
    ) -> Result<Value, BrowserError> {
        let expr = VISUAL_DIFF_JS
            .replace("__BASE__", base_b64)
            .replace("__CUR__", cur_b64);
        let out = self.backend.eval(target, &expr).await?;
        Ok(out.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Judge-scored UX review: gather page facts, ask the judge one Noul per
    /// dimension (clarity/hierarchy/affordance/consistency by default), and
    /// report the scores. Advisory by default (never fails the run); set
    /// `ux.gate=true` (+ optional `ux.min`) to fail when a dimension is low.
    /// Degrades to a skipped, passing check when no judge is configured or the
    /// judge is unreachable, so it never blocks a run on its own absence.
    async fn ux_check(&self, target: &str, spec: &Value) -> Value {
        let uo = &spec["ux"];
        let dims: Vec<String> = uo
            .get("dims")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| {
                ["clarity", "hierarchy", "affordance", "consistency"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect()
            });
        let gate = uo.get("gate").and_then(Value::as_bool).unwrap_or(false);
        let min = uo.get("min").and_then(Value::as_f64).unwrap_or(0.5);
        let skipped =
            |d: String| json!({ "name": "ux", "ok": true, "advisory": true, "detail": d });
        let Some(judge) = self.judge.as_ref() else {
            return skipped("judge not configured; skipped".into());
        };
        let facts = match self.backend.eval(target, UX_FACTS_JS).await {
            Ok(v) => v.get("result").cloned().unwrap_or_else(|| json!({})),
            Err(e) => {
                return skipped(format!(
                    "could not read page facts: {}",
                    browser_err_msg(&e)
                ))
            }
        };
        let mut qs = std::collections::BTreeMap::new();
        for d in &dims {
            qs.insert(
                d.clone(),
                mcp_judge::Question::Noul {
                    instructions: ux_instruction(d),
                    criteria: None,
                },
            );
        }
        match judge.ask(facts, qs).await {
            Ok(ans) => {
                let mut scores = serde_json::Map::new();
                let mut low = Vec::new();
                for d in &dims {
                    if let Some(mcp_judge::Answer::Noul { noul }) = ans.answers.get(d) {
                        let p = noul.clamp(0.0, 1.0);
                        scores.insert(d.clone(), json!((p * 100.0).round() / 100.0));
                        if p < min {
                            low.push(d.clone());
                        }
                    }
                }
                let ok = !gate || low.is_empty();
                json!({
                    "name": "ux",
                    "ok": ok,
                    "advisory": !gate,
                    "detail": format!("{} scored, {} below {:.2}", scores.len(), low.len(), min),
                    "scores": scores,
                    "low": low,
                })
            }
            Err(e) => skipped(format!("judge unavailable: {}", e.message())),
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
                    return Envelope::fail(
                        tool,
                        ErrorCode::InvalidArgs,
                        "save needs 'steps' array",
                    );
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
                    Ok(None) => {
                        Envelope::fail(tool, ErrorCode::NotFound, format!("no flow '{name}'"))
                    }
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
                        return Envelope::fail(
                            tool,
                            ErrorCode::NotFound,
                            format!("no flow '{name}'"),
                        )
                    }
                    Err(e) => return flow_err(tool, e),
                };
                let cont = args
                    .get("continue_on_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let secrets = match parse_secrets(args.get("secrets")) {
                    Ok(s) => s,
                    Err(m) => return Envelope::fail(tool, ErrorCode::InvalidArgs, m),
                };
                // Fail before any step runs: a login that types the username
                // and then stops for want of a password has already changed
                // the page.
                let missing = crate::flow::missing_secret_refs(&flow.steps, &secrets);
                if !missing.is_empty() {
                    let first = crate::flow::missing_msg(&missing[0]);
                    let msg = if missing.len() > 1 {
                        format!("{first}; also missing: {}", missing[1..].join(", "))
                    } else {
                        first
                    };
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, msg);
                }
                self.replay(tool, target, &flow, cont, &secrets).await
            }
            other => Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                format!("unknown action '{other}'"),
            ),
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
        secrets: &BTreeMap<String, String>,
    ) -> Envelope {
        let mut results = Vec::new();
        let mut passed = true;
        for (i, step) in flow.steps.iter().enumerate() {
            let (ok, detail) = match crate::flow::resolve_step_secrets(step, secrets) {
                // The resolved copy lives for this step only.
                Ok(resolved) => self.run_step(target, &resolved).await,
                Err(m) => (false, json!(m)),
            };
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
                // An older recording holds a placeholder in place of the
                // value: refuse loudly rather than type it.
                if str_arg(step, "value") == Some(crate::flow::SECRET_PLACEHOLDER) {
                    return (
                        false,
                        json!("act step holds a secret placeholder from an older recording: its value was never stored. Re-record it, or replace the step's value with \"secret_ref\": \"<name>\" and pass secrets:{\"<name>\": \"...\"} when running the flow"),
                    );
                }
                let locator = if let Some(r) = str_arg(step, "ref") {
                    crate::backend::Locator::Ref(r)
                } else if let Some(q) = str_arg(step, "query") {
                    crate::backend::Locator::Selector {
                        by: str_arg(step, "by").unwrap_or("css"),
                        query: q,
                        within: str_arg(step, "within"),
                        text: str_arg(step, "text"),
                        index: step
                            .get("index")
                            .and_then(Value::as_u64)
                            .map(|n| n as usize),
                    }
                } else {
                    return (false, json!("act step needs 'ref' or 'query' (with optional 'by', 'within', 'text', 'index')"));
                };
                // A secret value (substituted from `secrets` for this step) is
                // masked in anything that would display it, like `browser_act`.
                let secret = step.get("secret").and_then(Value::as_bool) == Some(true);
                self.backend
                    .act_masked(
                        target,
                        locator,
                        str_arg(step, "action").unwrap_or("click"),
                        str_arg(step, "value"),
                        secret,
                    )
                    .await
            }
            "dialog" => {
                // How the tab answers its JavaScript dialogs from here on (the
                // same standing policy as `browser_dialog`). A recording puts
                // one before the action that raised the dialog.
                let policy = match str_arg(step, "policy") {
                    Some("accept") => {
                        DialogPolicy::Accept(str_arg(step, "prompt_text").map(str::to_string))
                    }
                    Some("dismiss") => DialogPolicy::Dismiss,
                    _ => {
                        return (
                            false,
                            json!("dialog step needs 'policy': 'accept' or 'dismiss'"),
                        )
                    }
                };
                self.backend.dialog(target, Some(policy)).await
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
            "fill_form" => {
                let Some(fields) = step.get("fields") else {
                    return (false, json!("fill_form step needs 'fields'"));
                };
                self.backend
                    .fill_form(target, fields, step.get("submit"))
                    .await
            }
            "extract" => {
                let Some(schema) = step.get("schema") else {
                    return (false, json!("extract step needs 'schema'"));
                };
                self.backend
                    .extract(target, schema, str_arg(step, "within"))
                    .await
            }
            "wait" => {
                let (cond, arg) = if let Some(sel) = str_arg(step, "selector") {
                    ("selector", Some(sel))
                } else if step.get("dom_settled").is_some() {
                    ("dom_settled", None)
                } else if step.get("htmx_settled").is_some()
                    || str_arg(step, "condition") == Some("htmx_settled")
                {
                    ("htmx_settled", None)
                } else if step.get("navigation").is_some() {
                    ("navigation", None)
                } else {
                    ("network_idle", None)
                };
                let t = step
                    .get("timeout_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(10_000);
                let window = match step.get("navigation_timeout_ms") {
                    None | Some(Value::Null) => None,
                    Some(v) => match nav_window_arg(v) {
                        Some(n) => Some(n),
                        None => {
                            return (
                                false,
                                json!("navigation_timeout_ms must be an integer from 0 to 30000"),
                            )
                        }
                    },
                };
                self.backend.wait_window(target, cond, arg, t, window).await
            }
            "capture" => {
                self.backend
                    .capture(target, str_arg(step, "action").unwrap_or("start"), step)
                    .await
            }
            "assert" => match self.assert_full(target, step).await {
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

/// A `navigation_timeout_ms` value: an integer within the allowed window.
fn nav_window_arg(v: &Value) -> Option<u64> {
    v.as_u64()
        .filter(|n| *n <= crate::backend::NAV_EXPECT_MAX_MS)
}

/// A flow step that makes the tab answer its dialogs "yes".
fn is_accepting_dialog_step(step: &Value) -> bool {
    step.get("op").and_then(Value::as_str) == Some("dialog")
        && step.get("policy").and_then(Value::as_str) == Some("accept")
}

/// The `secrets` argument of a flow run: an object of string values. A wrong
/// shape is refused without echoing any value.
fn parse_secrets(v: Option<&Value>) -> Result<BTreeMap<String, String>, String> {
    let Some(v) = v.filter(|v| !v.is_null()) else {
        return Ok(BTreeMap::new());
    };
    let Some(obj) = v.as_object() else {
        return Err("'secrets' must be an object of name: value strings".into());
    };
    let mut out = BTreeMap::new();
    for (k, val) in obj {
        match val.as_str() {
            Some(s) => {
                out.insert(k.clone(), s.to_string());
            }
            None => return Err(format!("secret '{k}' must be a string")),
        }
    }
    Ok(out)
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
                "Attach to a Chromium browser started with --remote-debugging-port, or launch a dedicated instance. Optionally auto-restores a saved profile. The browser's active tab is brought to the front on connect (result 'foregrounded'). launch.browser='safari' drives Safari through safaridriver (macOS only, experimental: needs `safaridriver --enable` once, and a Safari that was already open when automation was enabled must be quit first; opens a visible window). On Safari, browser_network, browser_dialog, browser_viewport, browser_record, browser_branch, browser_checkpoint and browser_act 'press' return Unsupported.",
                obj(
                    json!({
                        "attach": { "type": "object", "properties": { "port": { "type": "integer" } } },
                        "launch": { "type": "object", "properties": {
                            "browser": { "type": "string", "enum": ["chromium", "safari"], "description": "chromium (default) launches an auto-discovered Chrome/Chromium; safari launches experimental Safari via safaridriver (macOS only)" },
                            "url": { "type": "string", "description": "first page to open; Safari only (Chromium: use browser_navigate); checked against the navigation policy" },
                            "port": { "type": "integer", "description": "remote-debugging port; omit or 0 to let Chrome pick a free one (the connect result reports it)" },
                            "headless": { "type": "boolean" },
                            "args": { "type": "array", "items": { "type": "string" }, "description": "Chromium only: extra command-line flags, each one entry written --name or --name=value (no spaces; at most 32). Only these are accepted: --window-size, --window-position, --start-maximized, --start-fullscreen, --force-device-scale-factor, --hide-scrollbars, --force-dark-mode, --lang, --accept-lang, --user-agent, --mute-audio, --autoplay-policy, --disable-gpu, --disable-extensions, --disable-notifications, --disable-default-apps, --disable-sync, --disable-search-engine-choice-screen, --use-fake-device-for-media-stream, --auto-open-devtools-for-tabs, --incognito, the three --disable-*background* flags, and --disable-features naming CalculateNativeWinOcclusion, Translate, MediaRouter, OptimizationHints, AutofillServerCommunication or PaintHolding (merged with agentctl's own)" },
                            "background_throttling": { "type": "boolean", "description": "Chromium only. A visible (headless=false) browser is started with flags that stop Chrome throttling timers, rendering and screen recording when its window is behind another or covered. Set true to leave Chrome's normal throttling on. Default false" },
                            "user_data_dir": { "type": "string" },
                            "profile": { "type": "string", "description": "saved profile name to auto-restore upon connecting" }
                        } },
                        "profile": { "type": "string", "description": "saved profile name to auto-restore upon connecting" }
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
                "Flatten a page into interactable node refs (dom/accessibility) or raw text. The web equivalent of get_ui_tree. \
                 A <canvas> gets child nodes (tag canvas-child) only if the page itself publishes its interactive regions \
                 via canvas.__agentctl_regions or a data-canvas-regions JSON attribute; any other canvas is an opaque node.",
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
                "Act on a DOM node: click, type, select, hover, focus, scroll_into_view, submit, press \
                 (value Enter, Escape or Tab, sent as a real key event to the focused node; Chrome only). \
                 A page-published canvas region (a canvas-child ref from browser_snapshot) supports only click and hover, \
                 sent as real mouse input at the region centre; other actions on it return Unsupported. \
                 Target it with 'ref' (from browser_query/snapshot) or, in one call, with \
                 'query' plus optional 'by' (css/xpath/text), 'within' (scoped container), 'text' (substring filter), and 'index'.",
                obj(
                    json!({
                        "target_id": { "type": "string" },
                        "ref": { "type": "string", "description": "a ref from browser_query/snapshot" },
                        "by": { "type": "string", "enum": ["css", "xpath", "text"], "description": "how to read 'query' (default css); used when no 'ref'" },
                        "query": { "type": "string", "description": "selector to resolve and act on in one call, instead of 'ref'" },
                        "within": { "type": "string", "description": "optional CSS/XPath root selector to scope query search" },
                        "text": { "type": "string", "description": "optional text substring filter to narrow matches" },
                        "index": { "type": "integer", "description": "optional 0-based match index if query matches multiple elements (default 0)" },
                        "action": { "type": "string", "enum": ["click", "type", "select", "hover", "focus", "scroll_into_view", "submit", "press"] },
                        "value": { "type": "string", "description": "text for type, option for select, or key name for press (Enter, Escape, Tab)" },
                        "secret": { "type": "boolean", "description": "the value is a secret: keep it out of the audit log and never show it in the showcase typing HUD (password and one-time-code fields are masked automatically)" }
                    }),
                    json!(["target_id", "action"]),
                ),
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_fill_form",
                Category::Browser,
                Tier::Standard,
                "Fill multiple form fields (input, select, checkbox, radio) in one call and \
                 optionally submit. Eliminates round-trips for registration or checkout forms.",
                obj(
                    json!({
                        "target_id": { "type": "string" },
                        "fields": {
                            "type": "array",
                            "items": { "type": "object" },
                            "description": "Array of fields: [{ref or selector, value, type, secret}]"
                        },
                        "submit": {
                            "type": "object",
                            "description": "Optional submit trigger: {ref or selector}"
                        }
                    }),
                    json!(["target_id", "fields"]),
                ),
            ),
            ToolDescriptor::new(
                "browser_extract",
                Category::Browser,
                Tier::Read,
                "Extract structured data directly from the page using a CSS/attribute schema \
                 (e.g. text values, lists, tables). Offloads extraction parsing from the LLM.",
                obj(
                    json!({
                        "target_id": { "type": "string" },
                        "schema": {
                            "type": "object",
                            "description": "Extraction schema mapping field names to rules {selector, attr, regex, multiple, fields}"
                        },
                        "within": {
                            "type": "string",
                            "description": "Optional CSS root selector to scope extraction"
                        }
                    }),
                    json!(["target_id", "schema"]),
                ),
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_profile",
                Category::Browser,
                Tier::Standard,
                "Save, restore, list, or delete browser session profiles (cookies, localStorage, \
                 sessionStorage) for instant user or auth state swapping without re-logging in.",
                obj(
                    json!({
                        "action": { "type": "string", "enum": ["save", "restore", "list", "delete"] },
                        "target_id": { "type": "string", "description": "save/restore: the tab to snapshot or populate" },
                        "name": { "type": "string", "description": "save/restore/delete: profile name" }
                    }),
                    json!(["action"]),
                ),
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_branch",
                Category::Browser,
                Tier::Standard,
                "Speculative browser context branching: fork an isolated background context from a \
                 tab ('create'), run trials without affecting the visible tab, commit winning state \
                 ('commit'), discard failed branches ('discard'), switch focus ('switch'), or list branches ('list'). \
                 Branches run in a separate browser context and fail with an error if one cannot be created \
                 (no silent fallback to the shared context). Commit and discard report an error unless the \
                 work was done and the branch tab was really closed. At most 8 branches may be active at once \
                 (AGENTCTL_MAX_BRANCHES). Chrome only: a Safari tab returns Unsupported.",
                obj(
                    json!({
                        "action": { "type": "string", "enum": ["create", "commit", "discard", "switch", "list"] },
                        "target_id": { "type": "string", "description": "create: parent tab to fork from" },
                        "branch_id": { "type": "string", "description": "create/commit/discard/switch: unique branch identifier" }
                    }),
                    json!(["action"]),
                ),
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_checkpoint",
                Category::Browser,
                Tier::Standard,
                "In-memory state checkpointing and rollback (T-1) for browser tabs. 'save' captures \
                 a deep copy of form state (input values, checks, select indexes, scroll), storage, \
                 cookies and URL (not the DOM tree); 'rollback' navigates back if needed (loading \
                 the page from the network with Chrome's HTTP cache bypassed, so a server that is \
                 down is an error, not a stale cached page; the result says cache_bypassed), waits \
                 for the page to load, restores that state and fails with the reason if any part \
                 could not be restored; 'list'/'delete' manage checkpoints. Re-saving a tag makes it the \
                 newest ('latest'). File inputs are skipped. Chrome only: a Safari tab returns Unsupported.",
                obj(
                    json!({
                        "action": { "type": "string", "enum": ["save", "rollback", "list", "delete"] },
                        "target_id": { "type": "string", "description": "target tab to checkpoint or restore" },
                        "tag": { "type": "string", "description": "save/rollback/delete: tag name (e.g. 'step_2' or 'latest')" }
                    }),
                    json!(["action"]),
                ),
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_wait",
                Category::Browser,
                Tier::Read,
                "Wait for a settle signal: a selector to appear, dom_settled (DOM mutations and \
                 animation frames settled for >=150ms), htmx_settled (HTMX requests and DOM swaps settled; errors if htmx is not present on the page), navigation to complete (after a goto, reload, click, submit or key press in this session it waits for the NEW document, not the one being left; a click that starts no navigation within navigation_timeout_ms, default 2s, settles on the loaded page with navigated:false; raise it for a handler that navigates later than that), the network to idle, or verification challenge clearance.",
                obj(
                    json!({
                        "target_id": { "type": "string" },
                        "selector": { "type": "string" },
                        "dom_settled": { "type": "boolean" },
                        "htmx_settled": { "type": "boolean" },
                        "navigation": { "type": "boolean" },
                        "network_idle": { "type": "boolean" },
                        "challenge_cleared": { "type": "boolean" },
                        "condition": { "type": "string", "enum": ["selector", "dom_settled", "htmx_settled", "navigation", "network_idle", "challenge_cleared"] },
                        "timeout_ms": { "type": "integer" },
                        "navigation_timeout_ms": { "type": "integer", "description": "navigation only: how long (ms, 0-30000, default 2000) to keep expecting a navigation that a click, submit or key press has not started yet, before settling on the loaded page with navigated:false. Does not apply after goto, reload, back or forward, which always navigate; timeout_ms still bounds the whole wait" }
                    }),
                    json!(["target_id"]),
                ),
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_challenge",
                Category::Browser,
                Tier::Standard,
                "Mixed-initiative CAPTCHA / 2FA detector and handshake. Pauses execution, shows a non-intrusive HUD in the browser informing the user to solve the verification, and auto-resumes in <=50ms upon clearance.",
                obj(
                    json!({
                        "target_id": { "type": "string" },
                        "action": { "type": "string", "enum": ["detect", "wait", "hud_show", "hud_hide"], "description": "action to perform (default: detect)" },
                        "timeout_ms": { "type": "integer", "description": "max wait time for human verification clearance in ms (default: 30000)" },
                        "kind": { "type": "string", "description": "optional challenge kind override for hud_show" }
                    }),
                    json!(["target_id"]),
                ),
            ),
            ToolDescriptor::new(
                "browser_record",
                Category::Browser,
                Tier::Standard,
                "Shadow observation & macro learning mode (Ghost Mode). Observes interactions in a tab (a person's, and the agent's own browser_act and browser_fill_form actions; events the page's own script fakes, such as el.click() or dispatchEvent, are ignored), across page loads and navigations (a link or form post becomes a wait for the next page, a typed URL or reload a goto), debounces keystrokes and click bursts, strips noise, and synthesizes clean, deterministic browser_flow steps. Secret fields are never recorded: they become steps with a secret_ref, supplied as secrets when the flow runs. Chrome only. JavaScript dialogs raised while recording are answered as 'dialogs' says: by the person at a visible window by default (the recording keeps the answer as a dialog step the flow replays before the action that raised it; a prompt's typed text is not kept), by the recorder in a headless browser (dismiss unless browser_dialog says accept).",
                obj(
                    json!({
                        "target_id": { "type": "string" },
                        "action": { "type": "string", "enum": ["start", "stop", "status"], "description": "recording action (default: status)" },
                        "name": { "type": "string", "description": "optional flow name to auto-save to flow store upon stop" },
                        "dialogs": { "type": "string", "enum": ["human", "accept", "dismiss"], "description": "start: who answers the page's JavaScript dialogs (confirm/prompt/alert/beforeunload) while recording. human: nobody does, so the person at the browser window answers and the recording keeps how they did (needs a visible browser; the default there). accept / dismiss: the recorder answers (dismiss, or the tab's browser_dialog policy, is the default for a headless browser). Every confirm/prompt/beforeunload becomes a dialog step in the flow" }
                    }),
                    json!(["target_id"]),
                ),
            ),
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
                "Evaluate arbitrary JavaScript in the page context. The result is the value of the last statement (a returned promise is awaited), JSON-serialized. Arbitrary code execution. If the script navigates the page the result is {navigated:true, value:null} rather than an error. On Safari, a page whose CSP forbids eval gets the code run without eval: an expression works as usual, but statements need an explicit `return` to produce a result.",
                obj(
                    json!({
                        "target_id": { "type": "string" },
                        "expression": { "type": "string" },
                        "timeout_ms": { "type": "integer", "description": "stop waiting after this many ms (default 10000, clamped to 100-60000). Chrome stops script that is still running; a timeout is an error that says so. Async work already scheduled (timers, pending promises) can keep running in the page. Not enforced on Safari" },
                        "detached": { "type": "boolean", "description": "start the script and return {started:true} without waiting for a promise it returns or for its result (Chrome only); its synchronous part still runs within the call and timeout_ms. A later rejection goes to the page console" }
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
                 allow-lists), component (role/visible/states of one element), visual (screenshot vs a \
                 saved baseline: first run saves it, later runs diff within tolerance), ux (judge-scored \
                 heuristics: clarity/hierarchy/affordance/consistency; advisory unless gate=true). 'within' \
                 scopes the DOM UX clauses to a component subtree. Settle first with wait_selector or \
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
                        "visual": { "description": "baseline name, or {name, tolerance, ref}; first run saves the baseline, later runs diff the screenshot within tolerance (default 0.01)" },
                        "ux": { "type": "object", "description": "judge-scored review: {dims:[clarity,hierarchy,affordance,consistency], gate:false, min:0.5}; advisory unless gate=true" },
                        "wait_selector": { "type": "string", "description": "settle: wait for this selector first" },
                        "wait_dom_settled": { "type": "boolean", "description": "settle: wait for DOM mutations to settle first" },
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
                 step is {op: navigate|act|wait|capture|assert|dialog, ...} using the same fields as \
                 those tools (e.g. {op:'act',by:'text',query:'Login',action:'click'}, \
                 {op:'assert',text:'Welcome'}). A secret step never holds its value: use \
                 {op:'act',action:'type',query:'#pw',secret:true,secret_ref:'pw'} and pass \
                 secrets:{pw:'...'} to 'run'; 'save' refuses a secret step with a literal value. \
                 A green run never needs a model.",
                obj(
                    json!({
                        "action": { "type": "string", "enum": ["save", "run", "list", "get", "delete"] },
                        "name": { "type": "string" },
                        "target_id": { "type": "string", "description": "run: the tab to replay against" },
                        "steps": { "type": "array", "items": { "type": "object" }, "description": "save: the ordered steps" },
                        "continue_on_error": { "type": "boolean", "description": "run: keep going past a failed step" },
                        "secrets": { "type": "object", "description": "run: values for the steps' secret_ref names, e.g. {pw: '...'}; used in memory for this run only, never stored, redacted from the audit log. A missing one fails the run before any step runs" }
                    }),
                    json!(["action"]),
                ),
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_showcase",
                Category::Browser,
                Tier::Standard,
                "Configure visual flair for demos, screencasts, and presentations: animated virtual SVG cursor, smooth cubic-bezier gliding, click ripples, and floating typing HUD, drawn inside the tab on Chrome and Safari. Decoration only: it never changes an action's result or error, and the typing HUD masks secrets and password/one-time-code fields.",
                obj(
                    json!({
                        "target_id": { "type": "string", "description": "the tab to configure showcase overlays for" },
                        "enabled": { "type": "boolean", "description": "enable or disable visual overlays" },
                        "speed": { "type": "string", "enum": ["cinematic", "demo", "snappy", "off"], "description": "gliding speed preset" },
                        "click_ripple": { "type": "boolean", "description": "expand glowing shockwave rings on click" },
                        "typing_hud": { "type": "boolean", "description": "display floating action/typing badges next to cursor" },
                        "cursor_style": { "type": "string", "enum": ["glow_arrow", "neon_cyan", "minimal_dot"], "description": "visual pointer style" },
                        "glide_ms": { "type": "integer", "description": "custom glide duration in milliseconds" }
                    }),
                    json!(["target_id"]),
                ),
            ).idempotent(true),
        ]
    }

    /// Answering a page's own confirmation on the user's behalf is a decision,
    /// not plumbing: `confirm("Delete this account?")` becomes "yes". Dismissal
    /// (the default) needs no approval because it is the null answer.
    fn consent_prompt(&self, name: &str, args: &Value) -> Option<String> {
        let accepts = match name {
            "browser_dialog" => args.get("policy").and_then(Value::as_str) == Some("accept"),
            // Recording that answers every dialog "yes" is the same decision.
            "browser_record" => {
                args.get("action").and_then(Value::as_str) == Some("start")
                    && args.get("dialogs").and_then(Value::as_str) == Some("accept")
            }
            // So is a flow step that does, whether it is being saved or run.
            "browser_flow" => {
                let steps = match args.get("action").and_then(Value::as_str) {
                    Some("save") => args.get("steps").and_then(Value::as_array).cloned(),
                    Some("run") => args
                        .get("name")
                        .and_then(Value::as_str)
                        .zip(self.flows.as_ref())
                        .and_then(|(n, s)| s.get(n).ok().flatten())
                        .map(|f| f.steps),
                    _ => None,
                };
                steps.is_some_and(|s| s.iter().any(is_accepting_dialog_step))
            }
            _ => false,
        };
        accepts.then(|| "Automatically ACCEPT JavaScript dialogs in this tab? Any confirm() the page raises will be answered 'yes' without further prompting.".to_string())
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
            "browser_fill_form" => self.fill_form(&args).await,
            "browser_extract" => self.extract(&args).await,
            "browser_profile" => self.profile(&args).await,
            "browser_wait" => self.wait(&args).await,
            "browser_challenge" => self.challenge(&args).await,
            "browser_record" => self.record(&args).await,
            "browser_showcase" => self.showcase(&args).await,
            "browser_screenshot" => self.screenshot(&args).await,
            "browser_viewport" => self.viewport(&args).await,
            "browser_eval" => self.eval(&args).await,
            "browser_dialog" => self.dialog(&args).await,
            "browser_network" => self.network(&args).await,
            "browser_cookies" => self.cookies(&args).await,
            "browser_capture" => self.capture(&args).await,
            "browser_assert" => self.assert(&args).await,
            "browser_flow" => self.flow(&args).await,
            "browser_branch" => self.branch(&args).await,
            "browser_checkpoint" => self.checkpoint(&args).await,
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
        forms: Mutex<Vec<Value>>,
        extracts: Mutex<Vec<Value>>,
        profiles: Mutex<Vec<String>>,
        /// `navigate` / `profile_restore` calls, in the order they happened.
        order: Mutex<Vec<String>>,
        connects: std::sync::atomic::AtomicUsize,
        fail_restore: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl BrowserBackend for Recorder {
        async fn connect(&self, _p: Option<u16>, _l: Option<Value>) -> Result<Value, BrowserError> {
            self.connects
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(json!({ "browser_id": 1 }))
        }
        async fn disconnect(&self, _b: u32, _k: bool) -> Result<Value, BrowserError> {
            Err(BrowserError::Failed("n/a".into()))
        }
        async fn tabs(
            &self,
            _b: u32,
            _a: &str,
            _t: Option<&str>,
            _u: Option<&str>,
        ) -> Result<Value, BrowserError> {
            Ok(json!({
                "tabs": [{"target_id": "T", "url": "about:blank"}]
            }))
        }
        async fn navigate(
            &self,
            _t: &str,
            a: &str,
            _u: Option<&str>,
        ) -> Result<Value, BrowserError> {
            self.order.lock().unwrap().push(format!("navigate:{a}"));
            Ok(json!({ "ok": true, "action": a }))
        }
        async fn snapshot(
            &self,
            _t: &str,
            _m: &str,
            _r: Option<&str>,
        ) -> Result<Value, BrowserError> {
            Err(BrowserError::Failed("n/a".into()))
        }
        async fn query(
            &self,
            _t: &str,
            _by: &str,
            _q: &str,
            _a: bool,
        ) -> Result<Value, BrowserError> {
            Err(BrowserError::Failed("n/a".into()))
        }
        async fn act(
            &self,
            _t: &str,
            locator: Locator<'_>,
            action: &str,
            _v: Option<&str>,
        ) -> Result<Value, BrowserError> {
            let desc = match locator {
                Locator::Ref(r) => format!("ref:{r}"),
                Locator::Selector {
                    by,
                    query,
                    within,
                    text,
                    index,
                } => {
                    let mut s = format!("sel:{by}:{query}");
                    if let Some(w) = within {
                        s.push_str(&format!(":within={w}"));
                    }
                    if let Some(t) = text {
                        s.push_str(&format!(":text={t}"));
                    }
                    if let Some(i) = index {
                        s.push_str(&format!(":index={i}"));
                    }
                    s
                }
            };
            self.acts.lock().unwrap().push(desc);
            Ok(json!({ "ok": true, "action": action }))
        }
        async fn wait(
            &self,
            _t: &str,
            c: &str,
            _a: Option<&str>,
            _ms: u64,
        ) -> Result<Value, BrowserError> {
            Ok(json!({ "settled": true, "condition": c }))
        }
        async fn screenshot(&self, _t: &str, _r: Option<&str>) -> Result<Shot, BrowserError> {
            Err(BrowserError::Failed("n/a".into()))
        }
        async fn eval(&self, _t: &str, _e: &str) -> Result<Value, BrowserError> {
            // Only the page-load probe is ever evaluated by the tool layer.
            Ok(json!({ "result": "https://example.com/app|complete" }))
        }
        async fn network(
            &self,
            _t: &str,
            _a: &str,
            _f: Option<&str>,
            _h: Option<Value>,
            _d: Option<u64>,
        ) -> Result<Value, BrowserError> {
            Err(BrowserError::Failed("n/a".into()))
        }
        async fn dialog(&self, _t: &str, _p: Option<DialogPolicy>) -> Result<Value, BrowserError> {
            Err(BrowserError::Failed("n/a".into()))
        }
        async fn cookies(
            &self,
            _t: &str,
            _a: &str,
            _c: Option<Value>,
        ) -> Result<Value, BrowserError> {
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
        async fn set_viewport(
            &self,
            _t: &str,
            w: u32,
            h: u32,
            m: bool,
            s: f64,
        ) -> Result<Value, BrowserError> {
            self.viewports
                .lock()
                .unwrap()
                .push(format!("{w}x{h} mobile={m} scale={s}"));
            Ok(json!({ "width": w, "height": h, "mobile": m }))
        }
        async fn fill_form(
            &self,
            _t: &str,
            fields: &Value,
            submit: Option<&Value>,
        ) -> Result<Value, BrowserError> {
            self.forms.lock().unwrap().push(fields.clone());
            let count = fields.as_array().map(|a| a.len()).unwrap_or(0);
            Ok(json!({ "filled": count, "submitted": submit.is_some() }))
        }
        async fn extract(
            &self,
            _t: &str,
            schema: &Value,
            within: Option<&str>,
        ) -> Result<Value, BrowserError> {
            self.extracts.lock().unwrap().push(schema.clone());
            Ok(json!({
                "data": { "extracted": true },
                "within": within
            }))
        }
        async fn profile_state(&self, _t: &str) -> Result<Value, BrowserError> {
            self.profiles.lock().unwrap().push("state".into());
            Ok(json!({
                "url": "https://example.com/app",
                "cookies": [{"name": "sid", "value": "xyz"}],
                "local_storage": {"token": "abc"},
                "session_storage": {}
            }))
        }
        async fn profile_restore(&self, _t: &str, state: &Value) -> Result<Value, BrowserError> {
            self.profiles.lock().unwrap().push("restore".into());
            self.order.lock().unwrap().push("restore".into());
            if self.fail_restore.load(std::sync::atomic::Ordering::SeqCst) {
                return Ok(json!({ "ok": false, "error": "SecurityError: storage denied" }));
            }
            let cookies = state
                .get("cookies")
                .and_then(Value::as_array)
                .map(|a| a.len())
                .unwrap_or(0);
            Ok(json!({
                "restored_cookies": cookies,
                "local_storage_keys": 1,
                "session_storage_keys": 0
            }))
        }
        async fn showcase(
            &self,
            target: &str,
            config: Option<crate::showcase::ShowcaseConfig>,
        ) -> Result<Value, BrowserError> {
            let cfg = config.unwrap_or_default();
            Ok(json!({
                "target_id": target,
                "enabled": cfg.enabled,
                "speed": cfg.speed.as_str(),
                "glide_ms": cfg.glide_ms(),
                "click_ripple": cfg.click_ripple,
                "typing_hud": cfg.typing_hud
            }))
        }
    }

    fn module() -> (BrowserModule, Arc<Recorder>) {
        let rec = Arc::new(Recorder::default());
        (BrowserModule::new(rec.clone()), rec)
    }

    fn module_with_flows(tag: &str) -> BrowserModule {
        use crate::flow::FlowStore;
        let mut p = std::env::temp_dir();
        p.push(format!(
            "agentctl-flowtool-{tag}-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&p);
        BrowserModule::new(Arc::new(Recorder::default())).with_flow_store(FlowStore::new(p, 50, 50))
    }

    #[tokio::test]
    async fn answering_dialogs_yes_needs_consent_however_it_is_asked_for() {
        let m = module_with_flows("dialog-consent");
        let asks = |name: &str, args: Value| m.consent_prompt(name, &args).is_some();
        // The direct tool, and both new ways to make every confirm "yes".
        assert!(asks("browser_dialog", json!({"policy": "accept"})));
        assert!(asks(
            "browser_record",
            json!({"action": "start", "dialogs": "accept"})
        ));
        let accepting = json!([{"op": "dialog", "policy": "accept", "type": "confirm"}]);
        assert!(asks(
            "browser_flow",
            json!({"action": "save", "name": "f", "steps": accepting})
        ));
        // A stored flow that accepts asks when it is run, not only when saved.
        let saved = m
            .call(
                "browser_flow",
                json!({"action": "save", "name": "yes", "steps": accepting}),
                &CallCtx::new("test", mcp_types::CancelToken::new()),
            )
            .await;
        assert!(saved.ok, "{saved:?}");
        assert!(asks(
            "browser_flow",
            json!({"action": "run", "name": "yes"})
        ));
        // Dismissing, the default, and a flow with no accepting step do not.
        assert!(!asks("browser_dialog", json!({"policy": "dismiss"})));
        assert!(!asks("browser_record", json!({"action": "start"})));
        assert!(!asks(
            "browser_record",
            json!({"action": "start", "dialogs": "dismiss"})
        ));
        assert!(!asks(
            "browser_flow",
            json!({"action": "save", "name": "f", "steps": [{"op": "dialog", "policy": "dismiss"}]})
        ));
        assert!(!asks(
            "browser_flow",
            json!({"action": "run", "name": "nope"})
        ));
    }

    fn module_with_profiles_and_rec(tag: &str) -> (BrowserModule, Arc<Recorder>) {
        use crate::profile::ProfileStore;
        let mut p = std::env::temp_dir();
        p.push(format!(
            "agentctl-profiletool-{tag}-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&p);
        let rec = Arc::new(Recorder::default());
        (
            BrowserModule::new(rec.clone()).with_profile_store(ProfileStore::new(p, 50)),
            rec,
        )
    }

    fn module_with_profiles(tag: &str) -> BrowserModule {
        module_with_profiles_and_rec(tag).0
    }

    #[tokio::test]
    async fn a_ref_locates_by_ref() {
        let (m, rec) = module();
        let e = m
            .act(&json!({"target_id":"T","ref":"/html/body[1]/button[1]","action":"click"}))
            .await;
        assert!(e.ok, "{e:?}");
        assert_eq!(rec.acts.lock().unwrap()[0], "ref:/html/body[1]/button[1]");
    }

    #[tokio::test]
    async fn a_query_locates_by_selector_in_one_call() {
        let (m, rec) = module();
        let e = m
            .act(&json!({"target_id":"T","by":"text","query":"Login","action":"click"}))
            .await;
        assert!(e.ok, "{e:?}");
        // No separate browser_query was needed: the selector reached act directly.
        assert_eq!(rec.acts.lock().unwrap()[0], "sel:text:Login");
    }

    #[tokio::test]
    async fn a_query_defaults_to_css_when_by_is_omitted() {
        let (m, rec) = module();
        let e = m
            .act(&json!({"target_id":"T","query":"#save","action":"click"}))
            .await;
        assert!(e.ok, "{e:?}");
        assert_eq!(rec.acts.lock().unwrap()[0], "sel:css:#save");
    }

    #[tokio::test]
    async fn a_query_with_scoping_forwards_within_text_and_index() {
        let (m, rec) = module();
        let e = m
            .act(&json!({
                "target_id": "T",
                "query": "button",
                "by": "css",
                "within": ".table-row",
                "text": "Submit",
                "index": 2,
                "action": "click"
            }))
            .await;
        assert!(e.ok, "{e:?}");
        assert_eq!(
            rec.acts.lock().unwrap()[0],
            "sel:css:button:within=.table-row:text=Submit:index=2"
        );
    }

    #[tokio::test]
    async fn flow_replays_act_step_with_compound_scoping() {
        let (m, rec) = module();
        let (ok, _) = m
            .run_step(
                "T",
                &json!({
                    "op": "act",
                    "query": "button.emr-btn",
                    "within": "table tbody tr:first-child",
                    "text": "Dispense",
                    "index": 0,
                    "action": "click"
                }),
            )
            .await;
        assert!(ok);
        assert_eq!(
            rec.acts.lock().unwrap()[0],
            "sel:css:button.emr-btn:within=table tbody tr:first-child:text=Dispense:index=0"
        );
    }

    #[tokio::test]
    async fn neither_ref_nor_query_is_an_invalid_argument_and_never_calls_the_backend() {
        let (m, rec) = module();
        let e = m.act(&json!({"target_id":"T","action":"click"})).await;
        assert!(!e.ok);
        assert_eq!(e.error.unwrap().code, ErrorCode::InvalidArgs);
        assert!(
            rec.acts.lock().unwrap().is_empty(),
            "backend must not be called"
        );
    }

    #[tokio::test]
    async fn capture_routes_the_action_and_defaults_to_read() {
        let (m, rec) = module();
        assert!(
            m.capture(&json!({"target_id":"T","action":"start"}))
                .await
                .ok
        );
        assert!(m.capture(&json!({"target_id":"T"})).await.ok); // default
        assert_eq!(*rec.captures.lock().unwrap(), vec!["start", "read"]);
    }

    #[tokio::test]
    async fn viewport_passes_dimensions_through_and_a_flow_step_reaches_the_backend() {
        let (m, rec) = module();
        assert!(
            m.viewport(&json!({"target_id":"T","width":390,"height":844,"mobile":true}))
                .await
                .ok
        );
        // A viewport step in a replayed flow reaches the same backend call.
        let (ok, _) = m
            .run_step("T", &json!({"op":"viewport","width":1280,"height":800}))
            .await;
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
        let run = m
            .flow(&json!({"action":"run","name":"login","target_id":"T"}))
            .await;
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
        let run = m
            .flow(&json!({"action":"run","name":"f","target_id":"T"}))
            .await;
        assert!(!run.ok, "a failing flow is an error");
        let d = run.data.unwrap();
        assert_eq!(d["passed"], false);
        assert_eq!(
            d["ran"], 2,
            "stops at the failing assert, third step not reached"
        );
    }

    #[tokio::test]
    async fn run_without_a_target_is_an_invalid_argument() {
        let m = module_with_flows("notgt");
        m.flow(&json!({"action":"save","name":"f","steps":[{"op":"navigate"}]}))
            .await;
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

    #[tokio::test]
    async fn fill_form_batches_fields_and_reports_count() {
        let (m, rec) = module();
        let e = m
            .fill_form(&json!({
                "target_id": "T",
                "fields": [
                    { "selector": "#username", "value": "alice" },
                    { "selector": "#password", "value": "secret" }
                ],
                "submit": { "selector": "#login-btn" }
            }))
            .await;
        assert!(e.ok, "{e:?}");
        assert_eq!(rec.forms.lock().unwrap().len(), 1);
        let d = e.data.unwrap();
        assert_eq!(d["filled"], 2);
        assert_eq!(d["submitted"], true);
    }

    #[tokio::test]
    async fn fill_form_without_fields_fails_invalid_args() {
        let (m, _) = module();
        let e = m.fill_form(&json!({ "target_id": "T" })).await;
        assert!(!e.ok);
        assert_eq!(e.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn extract_evaluates_schema() {
        let (m, rec) = module();
        let e = m
            .extract(&json!({
                "target_id": "T",
                "schema": { "title": "h1" },
                "within": ".content"
            }))
            .await;
        assert!(e.ok, "{e:?}");
        assert_eq!(rec.extracts.lock().unwrap().len(), 1);
        let d = e.data.unwrap();
        assert_eq!(d["data"]["extracted"], true);
        assert_eq!(d["within"], ".content");
    }

    #[tokio::test]
    async fn wait_accepts_network_idle() {
        let (m, _) = module();
        for args in [
            json!({ "target_id": "T", "network_idle": true }),
            json!({ "target_id": "T", "condition": "network_idle" }),
        ] {
            let e = m.wait(&args).await;
            assert!(e.ok, "{e:?}");
            assert_eq!(e.data.unwrap()["condition"], "network_idle");
        }
    }

    #[tokio::test]
    async fn wait_supports_dom_settled() {
        let (m, _) = module();
        let e = m
            .wait(&json!({
                "target_id": "T",
                "dom_settled": true,
                "timeout_ms": 3000
            }))
            .await;
        assert!(e.ok, "{e:?}");
        let d = e.data.unwrap();
        assert_eq!(d["condition"], "dom_settled");
    }

    #[tokio::test]
    async fn wait_supports_htmx_settled() {
        let (m, _) = module();
        let e = m
            .wait(&json!({
                "target_id": "T",
                "htmx_settled": true,
                "timeout_ms": 3000
            }))
            .await;
        assert!(e.ok, "{e:?}");
        let d = e.data.unwrap();
        assert_eq!(d["condition"], "htmx_settled");

        let e2 = m
            .wait(&json!({
                "target_id": "T",
                "condition": "htmx_settled"
            }))
            .await;
        assert!(e2.ok, "{e2:?}");
        assert_eq!(e2.data.unwrap()["condition"], "htmx_settled");
    }

    #[tokio::test]
    async fn connect_auto_restores_named_profile() {
        let (m, rec) = module_with_profiles_and_rec("autoload");
        let save_res = m
            .profile(&json!({
                "action": "save",
                "name": "qa_saved_session",
                "target_id": "T"
            }))
            .await;
        assert!(save_res.ok, "{save_res:?}");

        let conn_res = m
            .connect(&json!({
                "profile": "qa_saved_session"
            }))
            .await;
        assert!(conn_res.ok, "{conn_res:?}");
        let data = conn_res.data.unwrap();
        assert_eq!(data["profile_restored"], "qa_saved_session");
        assert!(rec
            .profiles
            .lock()
            .unwrap()
            .contains(&"restore".to_string()));
    }

    #[tokio::test]
    async fn connect_navigates_to_the_profile_url_before_restoring() {
        let (m, rec) = module_with_profiles_and_rec("order");
        let save = m
            .profile(&json!({ "action": "save", "name": "ordered", "target_id": "T" }))
            .await;
        assert!(save.ok, "{save:?}");
        rec.order.lock().unwrap().clear();
        let conn = m.connect(&json!({ "profile": "ordered" })).await;
        assert!(conn.ok, "{conn:?}");
        assert_eq!(conn.data.unwrap()["profile_restored"], "ordered");
        // Storage writes throw on about:blank, so the page must come first.
        assert_eq!(
            *rec.order.lock().unwrap(),
            vec!["navigate:goto".to_string(), "restore".to_string()]
        );
    }

    #[tokio::test]
    async fn connect_with_unknown_profile_errors_and_does_not_connect() {
        let (m, rec) = module_with_profiles_and_rec("unknown");
        let conn = m.connect(&json!({ "profile": "no_such_profile" })).await;
        assert!(!conn.ok, "{conn:?}");
        assert_eq!(conn.error.as_ref().unwrap().code, ErrorCode::NotFound);
        assert!(conn
            .error
            .as_ref()
            .unwrap()
            .message
            .contains("no_such_profile"));
        assert_eq!(rec.connects.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn connect_with_profile_but_no_store_errors() {
        let (m, rec) = module();
        let conn = m.connect(&json!({ "profile": "x" })).await;
        assert!(!conn.ok, "{conn:?}");
        assert_eq!(rec.connects.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn connect_reports_a_failed_restore_instead_of_claiming_it() {
        let (m, rec) = module_with_profiles_and_rec("restore_fail");
        let save = m
            .profile(&json!({ "action": "save", "name": "bad", "target_id": "T" }))
            .await;
        assert!(save.ok, "{save:?}");
        rec.fail_restore
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let conn = m.connect(&json!({ "profile": "bad" })).await;
        assert!(conn.ok, "{conn:?}");
        let data = conn.data.unwrap();
        assert_eq!(data["profile_restored"], json!(false));
        assert!(data["profile_restore_error"]
            .as_str()
            .unwrap()
            .contains("storage denied"));
        // The explicit restore action must not say "restored" either.
        let r = m
            .profile(&json!({ "action": "restore", "target_id": "T", "name": "bad" }))
            .await;
        assert!(!r.ok, "{r:?}");
    }

    #[tokio::test]
    async fn connect_auto_restores_named_profile_under_launch() {
        let (m, rec) = module_with_profiles_and_rec("autoload_launch");
        let save_res = m
            .profile(&json!({
                "action": "save",
                "name": "qa_launch_session",
                "target_id": "T"
            }))
            .await;
        assert!(save_res.ok, "{save_res:?}");

        let conn_res = m
            .connect(&json!({
                "launch": {
                    "headless": true,
                    "profile": "qa_launch_session"
                }
            }))
            .await;
        assert!(conn_res.ok, "{conn_res:?}");
        let data = conn_res.data.unwrap();
        assert_eq!(data["profile_restored"], "qa_launch_session");
        assert!(rec
            .profiles
            .lock()
            .unwrap()
            .contains(&"restore".to_string()));
    }

    #[tokio::test]
    async fn profile_save_list_restore_delete_flow() {
        let m = module_with_profiles("full_flow");
        let save = m
            .profile(&json!({
                "action": "save",
                "name": "login_session",
                "target_id": "T"
            }))
            .await;
        assert!(save.ok, "{save:?}");
        let d = save.data.unwrap();
        assert_eq!(d["saved"], true);
        assert_eq!(d["name"], "login_session");

        let list = m.profile(&json!({ "action": "list" })).await;
        assert!(list.ok, "{list:?}");
        let list_data = list.data.unwrap();
        let arr = list_data["profiles"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "login_session");

        let restore = m
            .profile(&json!({
                "action": "restore",
                "name": "login_session",
                "target_id": "T"
            }))
            .await;
        assert!(restore.ok, "{restore:?}");
        let restore_data = restore.data.unwrap();
        assert_eq!(restore_data["restored"], true);
        assert_eq!(restore_data["restored_cookies"], 1);

        let del = m
            .profile(&json!({
                "action": "delete",
                "name": "login_session"
            }))
            .await;
        assert!(del.ok, "{del:?}");
        assert_eq!(del.data.unwrap()["deleted"], true);

        let list_after = m.profile(&json!({ "action": "list" })).await;
        assert!(list_after.ok);
        assert_eq!(
            list_after.data.unwrap()["profiles"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }

    #[tokio::test]
    async fn profile_is_disabled_without_a_store() {
        let (m, _) = module();
        let e = m.profile(&json!({ "action": "list" })).await;
        assert!(!e.ok);
        assert_eq!(e.error.unwrap().code, ErrorCode::UnsupportedOs);
    }

    #[tokio::test]
    async fn flow_replays_fill_form_and_extract_steps() {
        let m = module_with_flows("forms_and_extract");
        let save = m
            .flow(&json!({
                "action": "save",
                "name": "pipeline",
                "steps": [
                    { "op": "navigate", "url": "https://x" },
                    { "op": "fill_form", "fields": [{ "selector": "#email", "value": "a@b.com" }] },
                    { "op": "wait", "dom_settled": true },
                    { "op": "extract", "schema": { "name": ".profile-name" } },
                    { "op": "assert", "_pass": true }
                ]
            }))
            .await;
        assert!(save.ok, "{save:?}");
        let run = m
            .flow(&json!({ "action": "run", "name": "pipeline", "target_id": "T" }))
            .await;
        assert!(run.ok, "{run:?}");
        let d = run.data.unwrap();
        assert_eq!(d["passed"], true);
        assert_eq!(d["ran"], 5);
    }

    #[tokio::test]
    async fn profile_restore_nonexistent_returns_not_found() {
        let m = module_with_profiles("notfound");
        let res = m
            .profile(&json!({
                "action": "restore",
                "name": "ghost_session",
                "target_id": "T"
            }))
            .await;
        assert!(!res.ok);
        assert_eq!(res.error.unwrap().code, ErrorCode::NotFound);
    }

    #[tokio::test]
    async fn profile_save_missing_name_fails() {
        let m = module_with_profiles("noname");
        let res = m
            .profile(&json!({
                "action": "save",
                "target_id": "T"
            }))
            .await;
        assert!(!res.ok);
        assert_eq!(res.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn profile_invalid_name_fails() {
        let m = module_with_profiles("badname");
        let res = m
            .profile(&json!({
                "action": "save",
                "name": "../escape",
                "target_id": "T"
            }))
            .await;
        assert!(!res.ok);
        assert_eq!(res.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn extract_without_schema_fails() {
        let (m, _) = module();
        let res = m.extract(&json!({ "target_id": "T" })).await;
        assert!(!res.ok);
        assert_eq!(res.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn fill_form_with_empty_fields_array_fails() {
        let (m, _) = module();
        let res = m
            .fill_form(&json!({ "target_id": "T", "fields": [] }))
            .await;
        assert!(!res.ok);
        assert_eq!(res.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn assert_with_wait_dom_settled_flag() {
        let (m, _) = module();
        let res = m
            .assert(&json!({
                "target_id": "T",
                "wait_dom_settled": true,
                "_pass": true
            }))
            .await;
        assert!(res.ok, "{res:?}");
        assert_eq!(res.data.unwrap()["passed"], true);
    }

    #[tokio::test]
    async fn profile_delete_nonexistent_returns_not_found() {
        let m = module_with_profiles("del_notfound");
        let res = m
            .profile(&json!({
                "action": "delete",
                "name": "ghost_profile"
            }))
            .await;
        assert!(!res.ok);
        assert_eq!(res.error.unwrap().code, ErrorCode::NotFound);
    }

    #[tokio::test]
    async fn profile_delete_missing_name_fails() {
        let m = module_with_profiles("del_noname");
        let res = m
            .profile(&json!({
                "action": "delete"
            }))
            .await;
        assert!(!res.ok);
        assert_eq!(res.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn profile_restore_missing_name_fails() {
        let m = module_with_profiles("restore_noname");
        let res = m
            .profile(&json!({
                "action": "restore",
                "target_id": "T"
            }))
            .await;
        assert!(!res.ok);
        assert_eq!(res.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn profile_unknown_action_fails() {
        let m = module_with_profiles("unknown_act");
        let res = m
            .profile(&json!({
                "action": "wipe_everything"
            }))
            .await;
        assert!(!res.ok);
        assert_eq!(res.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn fill_form_with_non_array_fields_fails() {
        let (m, _) = module();
        let res = m
            .fill_form(&json!({
                "target_id": "T",
                "fields": "not-an-array"
            }))
            .await;
        assert!(!res.ok);
        assert_eq!(res.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn wait_without_condition_fails() {
        let (m, _) = module();
        let res = m.wait(&json!({ "target_id": "T" })).await;
        assert!(!res.ok);
        assert_eq!(res.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn branch_create_missing_args_fails() {
        let (m, _) = module();
        let res = m.branch(&json!({ "action": "create" })).await;
        assert!(!res.ok);
        assert_eq!(res.error.unwrap().code, ErrorCode::InvalidArgs);

        let res = m
            .branch(&json!({ "action": "create", "target_id": "T" }))
            .await;
        assert!(!res.ok);
        assert_eq!(res.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn branch_unknown_action_fails() {
        let (m, _) = module();
        let res = m.branch(&json!({ "action": "warp_speed" })).await;
        assert!(!res.ok);
        assert_eq!(res.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn checkpoint_save_missing_target_fails() {
        let (m, _) = module();
        let res = m.checkpoint(&json!({ "action": "save" })).await;
        assert!(!res.ok);
        assert_eq!(res.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn checkpoint_unknown_action_fails() {
        let (m, _) = module();
        let res = m.checkpoint(&json!({ "action": "quantum_leap" })).await;
        assert!(!res.ok);
        assert_eq!(res.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn test_browser_showcase_toggle_and_config() {
        let (m, _) = module();
        let res = m
            .showcase(&json!({
                "target_id": "T",
                "enabled": true,
                "speed": "cinematic",
                "click_ripple": true,
                "typing_hud": true,
                "cursor_style": "glow_arrow"
            }))
            .await;
        assert!(res.ok, "{res:?}");
        let data = res.data.unwrap();
        assert_eq!(data["enabled"], true);
        assert_eq!(data["speed"], "cinematic");
        assert_eq!(data["glide_ms"], 350);
        assert_eq!(data["click_ripple"], true);
        assert_eq!(data["typing_hud"], true);
    }

    #[tokio::test]
    async fn test_browser_showcase_presets() {
        let (m, _) = module();
        let res = m
            .showcase(&json!({
                "target_id": "T",
                "speed": "snappy"
            }))
            .await;
        assert!(res.ok);
        let data = res.data.unwrap();
        assert_eq!(data["speed"], "snappy");
        assert_eq!(data["glide_ms"], 120);

        // Unknown speed returns invalid args
        let res = m
            .showcase(&json!({
                "target_id": "T",
                "speed": "supersonic"
            }))
            .await;
        assert!(!res.ok);
        assert_eq!(res.error.unwrap().code, ErrorCode::InvalidArgs);
    }
}
