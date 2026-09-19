//! Browser engine types, the `BrowserBackend` trait, and the real CDP-backed
//! implementation (`CdpBackend`). Unlike the a11y/input/window engines, the
//! backend is OS-independent (it only speaks TCP/HTTP/WebSocket), so the real
//! implementation lives here rather than in a per-OS crate.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::cdp::{http_json, CdpConn, DialogPolicy};
use crate::nav::NavPolicy;

/// Why a browser operation failed.
#[derive(Debug, Clone)]
pub enum BrowserError {
    PermissionDenied(String),
    NotFound(String),
    Unsupported(String),
    Timeout(String),
    Failed(String),
}

/// A screenshot result (base64 PNG). `width`/`height` are known only for
/// element captures (from the clip rect); `0` means "not measured".
#[derive(Debug, Clone)]
pub struct Shot {
    pub base64: String,
    pub width: u32,
    pub height: u32,
}

/// The browser control surface. One real implementation ([`CdpBackend`]); the
/// trait exists for the same module/engine symmetry the other categories use.
/// How `act` locates the element to act on: either a `ref` from a prior
/// snapshot/query, or a selector resolved server-side in the same call (so a
/// scripted click/type is one round trip, not query-then-act).
#[derive(Debug, Clone, Copy)]
pub enum Locator<'a> {
    Ref(&'a str),
    Selector { by: &'a str, query: &'a str },
}

#[async_trait]
pub trait BrowserBackend: Send + Sync {
    /// Attach to (or launch) a browser; returns a `browser_id`.
    async fn connect(
        &self,
        attach_port: Option<u16>,
        launch: Option<Value>,
    ) -> Result<Value, BrowserError>;
    /// Forget a browser. `kill` additionally stops one *this process started*
    /// and removes the temporary profile created for it; an attached browser is
    /// someone else's process and is never killed.
    async fn disconnect(&self, browser_id: u32, kill: bool) -> Result<Value, BrowserError>;
    /// Tab lifecycle: `list` / `open` / `activate` / `close`.
    async fn tabs(
        &self,
        browser_id: u32,
        action: &str,
        target_id: Option<&str>,
        url: Option<&str>,
    ) -> Result<Value, BrowserError>;
    /// Navigate a tab: `goto` / `back` / `forward` / `reload`.
    async fn navigate(
        &self,
        target: &str,
        action: &str,
        url: Option<&str>,
    ) -> Result<Value, BrowserError>;
    /// Flatten a page: `dom` / `accessibility` / `text`.
    async fn snapshot(
        &self,
        target: &str,
        mode: &str,
        root: Option<&str>,
    ) -> Result<Value, BrowserError>;
    /// Resolve node ref(s): `css` / `xpath` / `text`.
    async fn query(
        &self,
        target: &str,
        by: &str,
        query: &str,
        all: bool,
    ) -> Result<Value, BrowserError>;
    /// Act on a DOM node ref.
    async fn act(
        &self,
        target: &str,
        locator: Locator<'_>,
        action: &str,
        value: Option<&str>,
    ) -> Result<Value, BrowserError>;
    /// Wait for a settle signal (`selector` / `navigation` / `network_idle`).
    async fn wait(
        &self,
        target: &str,
        cond: &str,
        arg: Option<&str>,
        timeout_ms: u64,
    ) -> Result<Value, BrowserError>;
    /// Screenshot the page or one element.
    async fn screenshot(&self, target: &str, node_ref: Option<&str>) -> Result<Shot, BrowserError>;
    /// Evaluate arbitrary JS in the page (dangerous).
    async fn eval(&self, target: &str, expression: &str) -> Result<Value, BrowserError>;
    /// Network inspection/mutation (dangerous).
    async fn network(
        &self,
        target: &str,
        action: &str,
        filter: Option<&str>,
        headers: Option<Value>,
        duration_ms: Option<u64>,
    ) -> Result<Value, BrowserError>;
    /// Cookie access (dangerous; values redacted on read).
    /// Set how JavaScript dialogs on `target` are answered, and report the ones
    /// already seen.
    async fn dialog(
        &self,
        target: &str,
        policy: Option<DialogPolicy>,
    ) -> Result<Value, BrowserError>;
    async fn cookies(
        &self,
        target: &str,
        action: &str,
        cookie: Option<Value>,
    ) -> Result<Value, BrowserError>;
    /// Capture buffer for regression testing: `start` installs a page hook
    /// that records fetch/XHR (with bodies) and console errors/uncaught
    /// exceptions; `read` returns them; `clear` empties them.
    async fn capture(
        &self,
        target: &str,
        action: &str,
        opts: &Value,
    ) -> Result<Value, BrowserError>;
    /// Settle (optional) then evaluate assertions in one call, returning
    /// `{passed, checks}`. See the tool schema for the clauses.
    async fn assert(&self, target: &str, spec: &Value) -> Result<Value, BrowserError>;
    /// Release anything this backend started. Default: nothing was started.
    fn shutdown(&self) {}
}

#[derive(Clone)]
struct BrowserEntry {
    id: u32,
    host: String,
    port: u16,
}

/// A browser *this process started*, kept so it can be stopped again.
///
/// `std::process::Child` is not `Clone`, and dropping one does not kill the
/// process, so the handle lives in its own table rather than in the cloneable
/// `BrowserEntry`. `user_data_dir` is `Some` only when we chose the directory:
/// an operator-supplied profile is never deleted.
struct Launched {
    id: u32,
    child: std::process::Child,
    user_data_dir: Option<std::path::PathBuf>,
}

/// The real Chrome DevTools Protocol backend.
pub struct CdpBackend {
    browsers: Mutex<Vec<BrowserEntry>>,
    /// Browsers started by this process, by `browser_id`.
    launched: Mutex<Vec<Launched>>,
    next_id: AtomicU32,
    /// Where `goto` may take the browser: `browser.allowed_origins` plus the
    /// resolved-address check (see [`crate::nav`]).
    nav: NavPolicy,
    /// Per-target answer for JavaScript dialogs, and the log of ones answered.
    /// Connections are per-call, so the policy has to live with the backend.
    dialogs: Mutex<HashMap<String, (DialogPolicy, Vec<Value>)>>,
}

impl CdpBackend {
    pub fn new(nav: NavPolicy) -> Self {
        CdpBackend {
            browsers: Mutex::new(Vec::new()),
            launched: Mutex::new(Vec::new()),
            next_id: AtomicU32::new(1),
            nav,
            dialogs: Mutex::new(HashMap::new()),
        }
    }

    /// Stop every browser this process started and remove the profiles it
    /// created. Idempotent, so `Drop` and an explicit shutdown can both run.
    fn reap_all(&self) {
        let taken: Vec<Launched> = {
            let mut g = self.launched.lock().expect("launched mutex");
            std::mem::take(&mut *g)
        };
        for l in taken {
            tracing::info!(
                browser_id = l.id,
                "stopping browser launched by this session"
            );
            reap_one(l.child, l.user_data_dir.as_deref());
        }
    }

    /// Snapshot of connected browsers (guard released before any await).
    fn browsers(&self) -> Vec<BrowserEntry> {
        self.browsers.lock().expect("browsers mutex").clone()
    }

    /// Find a target's `webSocketDebuggerUrl` across all connected browsers.
    async fn resolve_ws(&self, target: &str) -> Result<String, BrowserError> {
        for b in self.browsers() {
            let list = match http_json(&b.host, b.port, "GET", "/json/list").await {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(arr) = list.as_array() {
                for t in arr {
                    if t.get("id").and_then(Value::as_str) == Some(target) {
                        if let Some(ws) = t.get("webSocketDebuggerUrl").and_then(Value::as_str) {
                            return Ok(ws.to_string());
                        }
                    }
                }
            }
        }
        Err(BrowserError::NotFound(format!(
            "target '{target}' not found in any connected browser"
        )))
    }

    async fn conn(&self, target: &str) -> Result<CdpConn, BrowserError> {
        let ws = self.resolve_ws(target).await?;
        let mut c = CdpConn::connect(&ws).await?;
        // Page must be enabled on *every* connection, not just the navigating
        // one: it is what routes `javascriptDialogOpening` to us. Without it an
        // alert() raised by browser_eval blocks the renderer with no client
        // able to clear it, and the tab stays dead for the rest of the session.
        c.call("Page.enable", json!({})).await.ok();
        c.set_dialog_policy(self.dialog_policy(target));
        Ok(c)
    }

    fn dialog_policy(&self, target: &str) -> DialogPolicy {
        self.dialogs
            .lock()
            .ok()
            .and_then(|m| m.get(target).map(|(p, _)| p.clone()))
            .unwrap_or_default()
    }

    /// Move any dialogs this connection answered into the target's log and onto
    /// the result, so an agent is told what it was asked even though the
    /// question was answered for it.
    fn note_dialogs(&self, target: &str, c: &mut CdpConn, out: &mut Value) {
        let seen = c.take_dialogs();
        if seen.is_empty() {
            return;
        }
        if let Ok(mut m) = self.dialogs.lock() {
            let entry = m.entry(target.to_string()).or_default();
            entry.1.extend(seen.iter().cloned());
            // Keep the log bounded; a page can raise dialogs in a loop.
            let len = entry.1.len();
            if len > 20 {
                entry.1.drain(..len - 20);
            }
        }
        if let Some(map) = out.as_object_mut() {
            map.insert("dialogs".into(), json!(seen));
        }
    }

    /// Run JS in the page and return the deserialized value (or a JS-exception
    /// error). Enables the Runtime domain first.
    async fn eval_value(c: &mut CdpConn, expr: &str) -> Result<Value, BrowserError> {
        c.call("Runtime.enable", json!({})).await.ok();
        let r = c
            .call(
                "Runtime.evaluate",
                json!({
                    "expression": expr,
                    "returnByValue": true,
                    "awaitPromise": true,
                    "userGesture": true
                }),
            )
            .await?;
        if let Some(exc) = r.get("exceptionDetails") {
            let text = exc
                .get("exception")
                .and_then(|e| e.get("description").or_else(|| e.get("value")))
                .and_then(Value::as_str)
                .or_else(|| exc.get("text").and_then(Value::as_str))
                .unwrap_or("javascript error");
            return Err(BrowserError::Failed(format!("eval: {text}")));
        }
        Ok(r.get("result")
            .and_then(|o| o.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }
}

/// JS helper: XPath of an element (id-anchored when possible).
const JS_XPATH: &str = r#"
function __xp(el){
  if(el && el.id) return '//*[@id="'+el.id+'"]';
  var parts=[];
  while(el && el.nodeType===1 && el.tagName!=='HTML'){
    var ix=1, sib=el.previousElementSibling;
    while(sib){ if(sib.tagName===el.tagName) ix++; sib=sib.previousElementSibling; }
    parts.unshift(el.tagName.toLowerCase()+'['+ix+']');
    el=el.parentElement;
  }
  return '/html/'+parts.join('/');
}
function __resolve(xp){
  var r=document.evaluate(xp, document, null, XPathResult.FIRST_ORDERED_NODE_TYPE, null);
  return r.singleNodeValue;
}
"#;

/// Resolve an element by selector, matching `browser_query`'s `by` values, for
/// act-by-selector. Text matches a leaf containing the string, then any element
/// whose exact trimmed text equals it (buttons, links).
const JS_FIND: &str = r#"
function __find(by,q){
  if(by==='css') return document.querySelector(q);
  if(by==='xpath'){ var r=document.evaluate(q,document,null,XPathResult.FIRST_ORDERED_NODE_TYPE,null); return r.singleNodeValue; }
  var w=document.querySelectorAll('*');
  for(var i=0;i<w.length;i++){ if(w[i].children.length===0 && (w[i].innerText||'').indexOf(q)>=0) return w[i]; }
  for(var j=0;j<w.length;j++){ if((w[j].textContent||'').trim()===q) return w[j]; }
  return null;
}
"#;

/// Page hook that records fetch/XHR (method, url, status, request+response
/// bodies, bounded) and console errors / uncaught exceptions into ring buffers
/// on `window.__agentctl`. Installed once per document; idempotent. Bodies can
/// contain secrets, which is why the tool that installs it is Dangerous-tier
/// and off unless the operator opts in.
const JS_CAPTURE_HOOK: &str = r#"
(function(){
  if(window.__agentctl_installed) return "already";
  window.__agentctl_installed=true;
  var CAP=200, BODY=4000, NET=[], CON=[];
  window.__agentctl={net:NET,con:CON};
  function pn(o){ if(NET.length>=CAP)NET.shift(); NET.push(o); }
  function pc(o){ if(CON.length>=CAP)CON.shift(); CON.push(o); }
  var of=window.fetch;
  if(of) window.fetch=function(input,init){
    var url=(input&&input.url)||input, method=(init&&init.method)||(input&&input.method)||'GET', rb=init&&init.body;
    return of.apply(this,arguments).then(function(r){
      var rec={t:'fetch',method:method,url:''+url,status:r.status,ok:r.ok,ts:Date.now()};
      if(typeof rb==='string') rec.reqBody=rb.slice(0,BODY);
      var rc=null; try{rc=r.clone();}catch(e){}
      if(rc){ rc.text().then(function(tx){ rec.respBody=(tx||'').slice(0,BODY); pn(rec); },function(){pn(rec);}); } else pn(rec);
      return r;
    },function(err){ pn({t:'fetch',method:method,url:''+url,status:0,ok:false,error:''+err,ts:Date.now()}); throw err; });
  };
  var OX=window.XMLHttpRequest;
  if(OX){ var NX=function(){ var x=new OX(),_o=x.open,_s=x.send,u,m,rb;
    x.open=function(mm,uu){m=mm;u=uu;return _o.apply(x,arguments);};
    x.send=function(b){ rb=b; x.addEventListener('loadend',function(){ var rec={t:'xhr',method:m,url:''+u,status:x.status,ok:x.status>=200&&x.status<300,ts:Date.now()}; if(typeof rb==='string')rec.reqBody=rb.slice(0,BODY); try{rec.respBody=(x.responseText||'').slice(0,BODY);}catch(e){} pn(rec); }); return _s.apply(x,arguments); };
    return x; }; NX.prototype=OX.prototype; window.XMLHttpRequest=NX; }
  ['error','warn'].forEach(function(lvl){ var o=console[lvl]; console[lvl]=function(){ try{pc({level:lvl,text:[].slice.call(arguments).map(String).join(' ').slice(0,BODY),ts:Date.now()});}catch(e){} return o.apply(console,arguments); }; });
  window.addEventListener('error',function(e){ pc({level:'uncaught',text:((e.message||'')+' @'+(e.filename||'')+':'+(e.lineno||'')).slice(0,BODY),ts:Date.now()}); });
  window.addEventListener('unhandledrejection',function(e){ pc({level:'unhandledrejection',text:String(e&&e.reason).slice(0,BODY),ts:Date.now()}); });
  return "installed";
})()
"#;

#[async_trait]
impl BrowserBackend for CdpBackend {
    async fn connect(
        &self,
        attach_port: Option<u16>,
        launch: Option<Value>,
    ) -> Result<Value, BrowserError> {
        let (host, port, started) = if let Some(p) = attach_port {
            ("127.0.0.1".to_string(), p, None)
        } else if let Some(spec) = launch {
            let (h, p, child, dir) = launch_browser(&spec).await?;
            (h, p, Some((child, dir)))
        } else {
            return Err(BrowserError::Failed(
                "browser_connect needs 'attach.port' or 'launch'".into(),
            ));
        };
        // Verify the endpoint is live. A browser we started but cannot reach is
        // reaped here rather than left behind by an early return.
        let ver = match http_json(&host, port, "GET", "/json/version").await {
            Ok(v) => v,
            Err(e) => {
                if let Some((child, dir)) = started {
                    reap_one(child, dir.as_deref());
                }
                return Err(BrowserError::Failed(format!(
                    "no CDP endpoint at {host}:{port} ({e:?})"
                )));
            }
        };
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        if let Some((child, user_data_dir)) = started {
            self.launched
                .lock()
                .expect("launched mutex")
                .push(Launched {
                    id,
                    child,
                    user_data_dir,
                });
        }
        self.browsers
            .lock()
            .expect("browsers mutex")
            .push(BrowserEntry {
                id,
                host: host.clone(),
                port,
            });
        Ok(json!({
            "browser_id": id,
            "host": host,
            "port": port,
            "browser": ver.get("Browser"),
            "protocol": ver.get("Protocol-Version"),
        }))
    }

    fn shutdown(&self) {
        self.reap_all();
    }

    async fn disconnect(&self, browser_id: u32, kill: bool) -> Result<Value, BrowserError> {
        let existed = {
            let mut g = self.browsers.lock().expect("browsers mutex");
            let before = g.len();
            g.retain(|b| b.id != browser_id);
            g.len() != before
        };
        if !existed {
            return Err(BrowserError::NotFound(format!(
                "no browser with id {browser_id}"
            )));
        }
        let mine = {
            let mut g = self.launched.lock().expect("launched mutex");
            g.iter()
                .position(|l| l.id == browser_id)
                .map(|i| g.remove(i))
        };
        let mut killed = false;
        let mut profile_removed = false;
        match (kill, mine) {
            (true, Some(l)) => {
                profile_removed = l.user_data_dir.is_some();
                reap_one(l.child, l.user_data_dir.as_deref());
                killed = true;
            }
            (true, None) => {
                return Err(BrowserError::Unsupported(
                    "this browser was attached, not launched by agentctl; \
                     'kill' only applies to browsers this session started"
                        .into(),
                ));
            }
            // Keep it running but stop tracking it, while still owning the
            // child, so shutdown reaps it instead of leaking the process.
            (false, Some(l)) => self.launched.lock().expect("launched mutex").push(l),
            (false, None) => {}
        }
        Ok(json!({
            "disconnected": browser_id,
            "killed": killed,
            "profile_removed": profile_removed,
        }))
    }

    async fn tabs(
        &self,
        browser_id: u32,
        action: &str,
        target_id: Option<&str>,
        url: Option<&str>,
    ) -> Result<Value, BrowserError> {
        let entry = self
            .browsers()
            .into_iter()
            .find(|b| b.id == browser_id)
            .ok_or_else(|| {
                BrowserError::NotFound(format!("browser_id {browser_id} not connected"))
            })?;
        let (host, port) = (entry.host.as_str(), entry.port);
        match action {
            "list" => {
                let list = http_json(host, port, "GET", "/json/list").await?;
                let tabs: Vec<Value> = list
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter(|t| t.get("type").and_then(Value::as_str) == Some("page"))
                            .map(|t| {
                                json!({
                                    "target_id": t.get("id"),
                                    "title": t.get("title"),
                                    "url": t.get("url"),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Ok(json!({ "tabs": tabs }))
            }
            "open" => {
                let u = url.unwrap_or("about:blank");
                let path = format!("/json/new?{u}");
                // Modern Chrome requires PUT; older builds accept GET.
                let r = match http_json(host, port, "PUT", &path).await {
                    Ok(v) => v,
                    Err(_) => http_json(host, port, "GET", &path).await?,
                };
                Ok(json!({ "target_id": r.get("id"), "url": r.get("url") }))
            }
            "activate" => {
                let t = target_id
                    .ok_or_else(|| BrowserError::Failed("activate needs target_id".into()))?;
                http_json(host, port, "GET", &format!("/json/activate/{t}")).await?;
                Ok(json!({ "activated": t }))
            }
            "close" => {
                let t = target_id
                    .ok_or_else(|| BrowserError::Failed("close needs target_id".into()))?;
                http_json(host, port, "GET", &format!("/json/close/{t}")).await?;
                Ok(json!({ "closed": t }))
            }
            other => Err(BrowserError::Failed(format!(
                "unknown tabs action '{other}'"
            ))),
        }
    }

    async fn navigate(
        &self,
        target: &str,
        action: &str,
        url: Option<&str>,
    ) -> Result<Value, BrowserError> {
        let mut c = self.conn(target).await?;
        let mut out = match action {
            "goto" => {
                let u = url.ok_or_else(|| BrowserError::Failed("goto needs 'url'".into()))?;
                if let Err(denied) = self.nav.check(u).await {
                    return Err(BrowserError::PermissionDenied(denied.message()));
                }
                let r = c.call("Page.navigate", json!({ "url": u })).await?;
                if let Some(err) = r.get("errorText").and_then(Value::as_str) {
                    return Err(BrowserError::Failed(format!("navigate: {err}")));
                }
                json!({ "url": u, "frameId": r.get("frameId") })
            }
            "reload" => {
                c.call("Page.reload", json!({})).await?;
                json!({ "reloaded": true })
            }
            "back" | "forward" => {
                let hist = c.call("Page.getNavigationHistory", json!({})).await?;
                let idx = hist
                    .get("currentIndex")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let empty = vec![];
                let entries = hist
                    .get("entries")
                    .and_then(Value::as_array)
                    .unwrap_or(&empty);
                let target_idx = if action == "back" { idx - 1 } else { idx + 1 };
                if target_idx < 0 || target_idx as usize >= entries.len() {
                    return Err(BrowserError::Failed(format!(
                        "no history entry to go {action}"
                    )));
                }
                let entry_id = entries[target_idx as usize]
                    .get("id")
                    .cloned()
                    .unwrap_or(json!(0));
                c.call(
                    "Page.navigateToHistoryEntry",
                    json!({ "entryId": entry_id }),
                )
                .await?;
                let u = entries[target_idx as usize].get("url").cloned();
                json!({ "url": u })
            }
            other => {
                return Err(BrowserError::Failed(format!(
                    "unknown navigate action '{other}'"
                )))
            }
        };
        // A `beforeunload` prompt fires here; dismissing it keeps the page,
        // so report it rather than let the navigation silently not happen.
        self.note_dialogs(target, &mut c, &mut out);
        Ok(out)
    }

    async fn snapshot(
        &self,
        target: &str,
        mode: &str,
        root: Option<&str>,
    ) -> Result<Value, BrowserError> {
        let mut c = self.conn(target).await?;
        if mode == "text" {
            let v = Self::eval_value(
                &mut c,
                "({url:location.href,title:document.title,text:(document.body?document.body.innerText:'').slice(0,20000)})",
            )
            .await?;
            return Ok(v);
        }
        // dom / accessibility both use a DOM flatten of interactable/labeled nodes.
        let root_arg = serde_json::to_string(&root).unwrap_or_else(|_| "null".into());
        let expr = format!(
            r#"(function(){{
  {JS_XPATH}
  var rootSel={root_arg};
  var base=(rootSel && document.querySelector(rootSel)) || document.body;
  if(!base) return {{url:location.href,title:document.title,nodes:[]}};
  var INTERACT={{A:1,BUTTON:1,INPUT:1,SELECT:1,TEXTAREA:1,SUMMARY:1,LABEL:1,OPTION:1}};
  var all=base.querySelectorAll('*'), out=[];
  for(var i=0;i<all.length && out.length<400;i++){{
    var el=all[i], tag=el.tagName, role=el.getAttribute('role');
    var interactive=INTERACT[tag]||role||el.getAttribute('tabindex')!==null||el.isContentEditable||typeof el.onclick==='function';
    if(!interactive) continue;
    var rect=el.getBoundingClientRect();
    if(rect.width===0 && rect.height===0) continue;
    var name=(el.getAttribute('aria-label')||el.getAttribute('placeholder')||el.value||el.innerText||el.getAttribute('title')||'').trim().slice(0,120);
    out.push({{ref:__xp(el),tag:tag.toLowerCase(),role:role||null,name:name,
      x:Math.round(rect.x),y:Math.round(rect.y),w:Math.round(rect.width),h:Math.round(rect.height)}});
  }}
  return {{url:location.href,title:document.title,mode:{mode:?},nodes:out}};
}})()"#
        );
        Self::eval_value(&mut c, &expr).await
    }

    async fn query(
        &self,
        target: &str,
        by: &str,
        query: &str,
        all: bool,
    ) -> Result<Value, BrowserError> {
        let mut c = self.conn(target).await?;
        let q = serde_json::to_string(query).unwrap_or_else(|_| "\"\"".into());
        let by_lit = serde_json::to_string(by).unwrap_or_else(|_| "\"css\"".into());
        let expr = format!(
            r#"(function(){{
  {JS_XPATH}
  var by={by_lit}, q={q}, all={all}, els=[];
  if(by==='css'){{ els=Array.from(document.querySelectorAll(q)); }}
  else if(by==='xpath'){{
    var r=document.evaluate(q,document,null,XPathResult.ORDERED_NODE_SNAPSHOT_TYPE,null);
    for(var i=0;i<r.snapshotLength;i++) els.push(r.snapshotItem(i));
  }} else {{ // text
    var w=document.querySelectorAll('*');
    for(var i=0;i<w.length;i++){{ if((w[i].innerText||'').indexOf(q)>=0 && w[i].children.length===0) els.push(w[i]); }}
  }}
  if(!all) els=els.slice(0,1);
  return els.slice(0,200).map(function(el){{
    var rect=el.getBoundingClientRect();
    return {{ref:__xp(el),tag:el.tagName.toLowerCase(),name:(el.innerText||el.value||'').trim().slice(0,120),
      x:Math.round(rect.x),y:Math.round(rect.y),w:Math.round(rect.width),h:Math.round(rect.height)}};
  }});
}})()"#
        );
        let v = Self::eval_value(&mut c, &expr).await?;
        let count = v.as_array().map(|a| a.len()).unwrap_or(0);
        Ok(json!({ "matches": v, "count": count }))
    }

    async fn act(
        &self,
        target: &str,
        locator: Locator<'_>,
        action: &str,
        value: Option<&str>,
    ) -> Result<Value, BrowserError> {
        let mut c = self.conn(target).await?;
        // Resolve to an element in the same eval: a `ref` via XPath, or a
        // selector via `__find`, so a scripted action is one round trip.
        let resolve = match locator {
            Locator::Ref(r) => {
                format!("__resolve({})", serde_json::to_string(r).unwrap_or_else(|_| "\"\"".into()))
            }
            Locator::Selector { by, query } => format!(
                "__find({},{})",
                serde_json::to_string(by).unwrap_or_else(|_| "\"css\"".into()),
                serde_json::to_string(query).unwrap_or_else(|_| "\"\"".into()),
            ),
        };
        let act = serde_json::to_string(action).unwrap_or_else(|_| "\"click\"".into());
        let val = serde_json::to_string(&value).unwrap_or_else(|_| "null".into());
        let expr = format!(
            r#"(function(){{
  {JS_XPATH}
  {JS_FIND}
  var el={resolve}, action={act}, value={val};
  if(!el) return {{ok:false,error:'element not found'}};
  try{{ el.scrollIntoView({{block:'center',inline:'center'}}); }}catch(e){{}}
  switch(action){{
    case 'click': el.click(); break;
    case 'focus': el.focus(); break;
    case 'hover': el.dispatchEvent(new MouseEvent('mouseover',{{bubbles:true}})); break;
    case 'scroll_into_view': break;
    case 'submit':
      if(el.form){{ el.form.requestSubmit?el.form.requestSubmit():el.form.submit(); }}
      else if(typeof el.submit==='function'){{ el.submit(); }}
      else return {{ok:false,error:'element has no form to submit'}};
      break;
    case 'select':
      el.value=value; el.dispatchEvent(new Event('change',{{bubbles:true}})); break;
    case 'type':
      if(el.focus) el.focus();
      if('value' in el){{ el.value=value; }} else {{ el.textContent=value; }}
      el.dispatchEvent(new Event('input',{{bubbles:true}}));
      el.dispatchEvent(new Event('change',{{bubbles:true}}));
      break;
    default: return {{ok:false,error:'unknown action '+action}};
  }}
  return {{ok:true,action:action}};
}})()"#
        );
        let mut v = Self::eval_value(&mut c, &expr).await?;
        if v.get("ok").and_then(Value::as_bool) == Some(false) {
            let msg = v
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("action failed");
            return Err(BrowserError::NotFound(msg.to_string()));
        }
        self.note_dialogs(target, &mut c, &mut v);
        Ok(v)
    }

    async fn wait(
        &self,
        target: &str,
        cond: &str,
        arg: Option<&str>,
        timeout_ms: u64,
    ) -> Result<Value, BrowserError> {
        use tokio::time::{sleep, Duration, Instant};
        let mut c = self.conn(target).await?;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.clamp(50, 60_000));
        let probe = match cond {
            "selector" => {
                let s =
                    arg.ok_or_else(|| BrowserError::Failed("wait selector needs a value".into()))?;
                let sl = serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
                format!("!!document.querySelector({sl})")
            }
            "navigation" => "document.readyState==='complete'".to_string(),
            "network_idle" => "document.readyState==='complete'".to_string(),
            other => {
                return Err(BrowserError::Failed(format!(
                    "unknown wait condition '{other}'"
                )))
            }
        };
        loop {
            let hit = Self::eval_value(&mut c, &probe).await?;
            if hit.as_bool() == Some(true) {
                // network_idle: require a short additional quiet window.
                if cond == "network_idle" {
                    sleep(Duration::from_millis(400)).await;
                }
                let mut out = json!({ "settled": true, "condition": cond });
                self.note_dialogs(target, &mut c, &mut out);
                return Ok(out);
            }
            if Instant::now() >= deadline {
                return Err(BrowserError::Timeout(format!(
                    "wait '{cond}' did not settle in {timeout_ms}ms"
                )));
            }
            sleep(Duration::from_millis(150)).await;
        }
    }

    async fn screenshot(&self, target: &str, node_ref: Option<&str>) -> Result<Shot, BrowserError> {
        let mut c = self.conn(target).await?;
        c.call("Page.enable", json!({})).await.ok();
        let mut params = json!({ "format": "png", "captureBeyondViewport": false });
        let (mut w, mut h) = (0u32, 0u32);
        if let Some(r) = node_ref {
            let xp = serde_json::to_string(r).unwrap_or_else(|_| "\"\"".into());
            let expr = format!(
                r#"(function(){{
  {JS_XPATH}
  var el=__resolve({xp}); if(!el) return null;
  el.scrollIntoView({{block:'center'}});
  var b=el.getBoundingClientRect();
  return {{x:b.x,y:b.y,w:b.width,h:b.height}};
}})()"#
            );
            let clip = Self::eval_value(&mut c, &expr).await?;
            if clip.is_null() {
                return Err(BrowserError::NotFound(format!("ref '{r}' not found")));
            }
            let gx = clip.get("x").and_then(Value::as_f64).unwrap_or(0.0);
            let gy = clip.get("y").and_then(Value::as_f64).unwrap_or(0.0);
            let gw = clip.get("w").and_then(Value::as_f64).unwrap_or(0.0);
            let gh = clip.get("h").and_then(Value::as_f64).unwrap_or(0.0);
            w = gw as u32;
            h = gh as u32;
            params["clip"] = json!({ "x": gx, "y": gy, "width": gw, "height": gh, "scale": 1 });
        }
        let r = c.call("Page.captureScreenshot", params).await?;
        let data = r
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| BrowserError::Failed("captureScreenshot returned no data".into()))?;
        Ok(Shot {
            base64: data.to_string(),
            width: w,
            height: h,
        })
    }

    async fn eval(&self, target: &str, expression: &str) -> Result<Value, BrowserError> {
        let mut c = self.conn(target).await?;
        let v = Self::eval_value(&mut c, expression).await?;
        let mut out = json!({ "result": v });
        self.note_dialogs(target, &mut c, &mut out);
        Ok(out)
    }

    async fn dialog(
        &self,
        target: &str,
        policy: Option<DialogPolicy>,
    ) -> Result<Value, BrowserError> {
        let mut m = self
            .dialogs
            .lock()
            .map_err(|_| BrowserError::Failed("dialog state poisoned".into()))?;
        let entry = m.entry(target.to_string()).or_default();
        if let Some(p) = policy {
            entry.0 = p;
        }
        let (accept, prompt_text) = match &entry.0 {
            DialogPolicy::Dismiss => (false, None),
            DialogPolicy::Accept(t) => (true, t.clone()),
        };
        Ok(json!({
            "policy": if accept { "accept" } else { "dismiss" },
            "prompt_text": prompt_text,
            "seen": entry.1,
        }))
    }

    async fn network(
        &self,
        target: &str,
        action: &str,
        filter: Option<&str>,
        headers: Option<Value>,
        duration_ms: Option<u64>,
    ) -> Result<Value, BrowserError> {
        let mut c = self.conn(target).await?;
        match action {
            "set_headers" => {
                let h = headers
                    .ok_or_else(|| BrowserError::Failed("set_headers needs 'headers'".into()))?;
                c.call("Network.enable", json!({})).await.ok();
                c.call("Network.setExtraHTTPHeaders", json!({ "headers": h }))
                    .await?;
                Ok(json!({ "ok": true, "applied": "extra_http_headers" }))
            }
            "log" => {
                c.call("Network.enable", json!({})).await.ok();
                let ms = duration_ms.unwrap_or(3000).clamp(100, 30_000);
                let events = c
                    .collect_events(
                        &[
                            "Network.requestWillBeSent",
                            "Network.responseReceived",
                            "Network.loadingFailed",
                        ],
                        ms,
                        400,
                    )
                    .await?;
                let mut requests: Vec<Value> = Vec::new();
                for e in &events {
                    let p = e.get("params").cloned().unwrap_or_else(|| json!({}));
                    let row = match e.get("method").and_then(Value::as_str) {
                        Some("Network.requestWillBeSent") => json!({
                            "phase": "request",
                            "url": p.pointer("/request/url"),
                            "method": p.pointer("/request/method"),
                            "type": p.get("type"),
                        }),
                        Some("Network.responseReceived") => json!({
                            "phase": "response",
                            "url": p.pointer("/response/url"),
                            "status": p.pointer("/response/status"),
                            "mime": p.pointer("/response/mimeType"),
                        }),
                        _ => json!({
                            "phase": "failed",
                            "error": p.get("errorText"),
                            "type": p.get("type"),
                        }),
                    };
                    // Header values and cookies are deliberately not copied:
                    // a request log is exactly where a session token would sit.
                    if let Some(f) = filter {
                        if !serde_json::to_string(&row).unwrap_or_default().contains(f) {
                            continue;
                        }
                    }
                    requests.push(row);
                }
                Ok(json!({
                    "requests": requests, "count": requests.len(),
                    "window_ms": ms, "truncated": events.len() >= 400,
                }))
            }
            "intercept" => {
                // Blocking by URL pattern is what "intercept" can mean without
                // holding requests open across tool calls: a paused request
                // with nobody to resume it stalls the page indefinitely.
                let patterns: Vec<String> = headers
                    .as_ref()
                    .and_then(|v| v.get("block"))
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                c.call("Network.enable", json!({})).await.ok();
                c.call("Network.setBlockedURLs", json!({ "urls": patterns }))
                    .await?;
                Ok(json!({
                    "blocked_patterns": patterns, "count": patterns.len(),
                    "note": if patterns.is_empty() { "blocking cleared" } else { "patterns applied to this tab" },
                }))
            }
            other => Err(BrowserError::Failed(format!(
                "unknown network action '{other}'"
            ))),
        }
    }

    async fn cookies(
        &self,
        target: &str,
        action: &str,
        cookie: Option<Value>,
    ) -> Result<Value, BrowserError> {
        let mut c = self.conn(target).await?;
        c.call("Network.enable", json!({})).await.ok();
        match action {
            "get" => {
                let r = c.call("Network.getCookies", json!({})).await?;
                let empty = vec![];
                let cookies = r.get("cookies").and_then(Value::as_array).unwrap_or(&empty);
                // Redact values: session tokens must never reach the agent/audit (D6).
                let redacted: Vec<Value> = cookies
                    .iter()
                    .map(|ck| {
                        json!({
                            "name": ck.get("name"),
                            "domain": ck.get("domain"),
                            "path": ck.get("path"),
                            "secure": ck.get("secure"),
                            "httpOnly": ck.get("httpOnly"),
                            "value": "***REDACTED***",
                        })
                    })
                    .collect();
                Ok(json!({ "cookies": redacted, "count": redacted.len() }))
            }
            "set" => {
                let ck = cookie.ok_or_else(|| BrowserError::Failed("set needs 'cookie'".into()))?;
                c.call("Network.setCookie", ck).await?;
                Ok(json!({ "ok": true }))
            }
            "clear" => {
                c.call("Network.clearBrowserCookies", json!({})).await?;
                Ok(json!({ "ok": true, "cleared": true }))
            }
            other => Err(BrowserError::Failed(format!(
                "unknown cookies action '{other}'"
            ))),
        }
    }

    async fn capture(
        &self,
        target: &str,
        action: &str,
        opts: &Value,
    ) -> Result<Value, BrowserError> {
        let mut c = self.conn(target).await?;
        match action {
            "start" => {
                // Persist across navigations, and cover the page already open.
                c.call("Page.enable", json!({})).await.ok();
                c.call(
                    "Page.addScriptToEvaluateOnNewDocument",
                    json!({ "source": JS_CAPTURE_HOOK }),
                )
                .await
                .ok();
                let now = Self::eval_value(&mut c, JS_CAPTURE_HOOK).await?;
                Ok(json!({ "ok": true, "current_page": now }))
            }
            "clear" => {
                Self::eval_value(
                    &mut c,
                    "(function(){if(window.__agentctl){window.__agentctl.net.length=0;window.__agentctl.con.length=0;}return true;})()",
                )
                .await?;
                Ok(json!({ "ok": true, "cleared": true }))
            }
            "read" => {
                let only_errors = opts.get("only_errors").and_then(Value::as_bool).unwrap_or(false);
                let filter = opts.get("filter").and_then(Value::as_str).unwrap_or("");
                let armed = Self::eval_value(&mut c, "!!window.__agentctl_installed").await?;
                if armed.as_bool() != Some(true) {
                    return Err(BrowserError::Failed(
                        "capture is not armed on this page; call browser_capture action='start' first".into(),
                    ));
                }
                let buf = Self::eval_value(
                    &mut c,
                    "JSON.stringify(window.__agentctl||{net:[],con:[]})",
                )
                .await?;
                // eval returns the JSON string; parse it back to structured data.
                let parsed: Value = buf
                    .as_str()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or(buf);
                let empty = vec![];
                let net = parsed.get("net").and_then(Value::as_array).unwrap_or(&empty);
                let con = parsed.get("con").and_then(Value::as_array).unwrap_or(&empty);
                let keep = |row: &Value, want_bad: bool| -> bool {
                    if !filter.is_empty()
                        && !serde_json::to_string(row).unwrap_or_default().contains(filter)
                    {
                        return false;
                    }
                    if want_bad {
                        return row.get("ok").and_then(Value::as_bool) == Some(false);
                    }
                    true
                };
                let net: Vec<Value> = net.iter().filter(|r| keep(r, only_errors)).cloned().collect();
                let con: Vec<Value> = con
                    .iter()
                    .filter(|r| filter.is_empty() || serde_json::to_string(r).unwrap_or_default().contains(filter))
                    .cloned()
                    .collect();
                Ok(json!({
                    "network": net,
                    "console": con,
                    "network_count": net.len(),
                    "console_count": con.len(),
                }))
            }
            other => Err(BrowserError::Failed(format!(
                "unknown capture action '{other}' (use start|read|clear)"
            ))),
        }
    }

    async fn assert(&self, target: &str, spec: &Value) -> Result<Value, BrowserError> {
        let timeout = spec
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(8000);
        let mut settle: Option<Value> = None;
        // Optional settle before checking, so an assertion right after an
        // action does not read the pre-action DOM.
        if let Some(sel) = spec.get("wait_selector").and_then(Value::as_str) {
            if let Err(e) = self.wait(target, "selector", Some(sel), timeout).await {
                settle = Some(json!({ "name": "wait_selector", "ok": false, "detail": berr_msg(&e) }));
            }
        } else if spec.get("wait_network_idle").and_then(Value::as_bool) == Some(true) {
            if let Err(e) = self.wait(target, "network_idle", None, timeout).await {
                settle = Some(json!({ "name": "wait_network_idle", "ok": false, "detail": berr_msg(&e) }));
            }
        }
        let mut c = self.conn(target).await?;
        let spec_lit = serde_json::to_string(spec).unwrap_or_else(|_| "{}".into());
        let expr = format!(
            r#"(function(){{
  var spec={spec_lit}, checks=[], A=window.__agentctl;
  var body=document.body?document.body.innerText:'';
  if(spec.text!=null) checks.push({{name:'text',ok:body.indexOf(spec.text)>=0,detail:spec.text}});
  if(spec.not_text!=null) checks.push({{name:'not_text',ok:body.indexOf(spec.not_text)<0,detail:spec.not_text}});
  if(spec.url!=null) checks.push({{name:'url',ok:location.href.indexOf(spec.url)>=0,detail:location.href}});
  if(spec.selector!=null){{var n=document.querySelectorAll(spec.selector).length;var min=spec.min_count||1;checks.push({{name:'selector',ok:n>=min,detail:spec.selector+' -> '+n+' (min '+min+')'}});}}
  if(spec.no_console_errors){{ if(!A){{checks.push({{name:'no_console_errors',ok:false,detail:'capture not armed; call browser_capture start first'}});}} else {{ var errs=A.con.filter(function(x){{return x.level==='error'||x.level==='uncaught'||x.level==='unhandledrejection';}}); checks.push({{name:'no_console_errors',ok:errs.length===0,detail:errs.length+' error(s)'}}); }} }}
  if(spec.no_failed_requests){{ if(!A){{checks.push({{name:'no_failed_requests',ok:false,detail:'capture not armed; call browser_capture start first'}});}} else {{ var bad=A.net.filter(function(x){{return x.ok===false;}}); checks.push({{name:'no_failed_requests',ok:bad.length===0,detail:bad.length+' failed'}}); }} }}
  return {{checks:checks}};
}})()"#
        );
        let result = Self::eval_value(&mut c, &expr).await?;
        let mut checks: Vec<Value> = result
            .get("checks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if let Some(s) = settle {
            checks.insert(0, s);
        }
        let passed = checks
            .iter()
            .all(|c| c.get("ok").and_then(Value::as_bool) == Some(true));
        if checks.is_empty() {
            return Err(BrowserError::Failed(
                "no assertions given (use text/not_text/url/selector/no_console_errors/no_failed_requests)".into(),
            ));
        }
        Ok(json!({ "passed": passed, "checks": checks }))
    }
}

/// The message inside a [`BrowserError`], for embedding in an assertion check.
fn berr_msg(e: &BrowserError) -> String {
    match e {
        BrowserError::PermissionDenied(m)
        | BrowserError::NotFound(m)
        | BrowserError::Unsupported(m)
        | BrowserError::Timeout(m)
        | BrowserError::Failed(m) => m.clone(),
    }
}

/// Common macOS/Linux Chromium binary locations, tried in order.
pub const CHROME_BINS: &[&str] = &[
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
    "/usr/bin/google-chrome",
    "/usr/bin/chromium",
    "/usr/bin/chromium-browser",
];

/// Last line of defence: a server that exits without calling `shutdown` still
/// takes its browsers with it. Modelled on `mcp_pty::PtySession`, which kills
/// its process group the same way.
impl Drop for CdpBackend {
    fn drop(&mut self) {
        self.reap_all();
    }
}

/// The temp profile directory this process would create for `port`.
///
/// Keyed on the pid as well as the port so two concurrent servers never share
/// a profile, and so ownership is decidable: only a directory matching this
/// shape, for *our* pid, was created by us and may be deleted.
fn own_profile_dir(port: u16) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("agentctl-cdp-{}-{port}", std::process::id()))
}

/// Stop one launched browser and remove the profile directory we created for
/// it. Best-effort throughout: this runs on shutdown paths where the only
/// alternative to ignoring an error is leaking the process.
fn reap_one(mut child: std::process::Child, user_data_dir: Option<&std::path::Path>) {
    let _ = child.kill();
    let _ = child.wait();
    if let Some(dir) = user_data_dir {
        if let Err(e) = std::fs::remove_dir_all(dir) {
            tracing::debug!(dir = %dir.display(), error = %e, "could not remove browser profile");
        }
    }
}

/// Launch a dedicated Chromium instance with a debugging port and poll until
/// its CDP endpoint answers.
///
/// Returns the child handle so the caller can stop it again: a dropped
/// `Child` does **not** kill the process, so discarding it leaks a browser and
/// its profile directory for the life of the machine.
type Launch = (String, u16, std::process::Child, Option<std::path::PathBuf>);

async fn launch_browser(spec: &Value) -> Result<Launch, BrowserError> {
    use tokio::time::{sleep, Duration};
    let port = spec.get("port").and_then(Value::as_u64).unwrap_or(9333) as u16;
    let headless = spec
        .get("headless")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // Only a directory we chose is ours to delete later.
    let (user_data_dir, owned) = match spec.get("user_data_dir").and_then(Value::as_str) {
        Some(p) => (std::path::PathBuf::from(p), None),
        None => {
            let d = own_profile_dir(port);
            (d.clone(), Some(d))
        }
    };
    let user_data_dir = user_data_dir.to_string_lossy().into_owned();
    let bin = CHROME_BINS
        .iter()
        .find(|p| std::path::Path::new(p).exists())
        .ok_or_else(|| {
            BrowserError::NotFound(
                "no Chromium binary found; attach to a running browser instead".into(),
            )
        })?;

    let mut cmd = std::process::Command::new(bin);
    cmd.arg(format!("--remote-debugging-port={port}"))
        .arg(format!("--user-data-dir={user_data_dir}"))
        .arg("--no-first-run")
        .arg("--no-default-browser-check");
    if headless {
        cmd.arg("--headless=new");
    }
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let child = cmd
        .spawn()
        .map_err(|e| BrowserError::Failed(format!("spawn {bin}: {e}")))?;

    // Poll for readiness (~8s).
    for _ in 0..40 {
        if http_json("127.0.0.1", port, "GET", "/json/version")
            .await
            .is_ok()
        {
            return Ok(("127.0.0.1".to_string(), port, child, owned));
        }
        sleep(Duration::from_millis(200)).await;
    }
    // It never came up, so nothing else will ever hold this handle.
    reap_one(child, owned.as_deref());
    Err(BrowserError::Timeout(format!(
        "launched browser but CDP port {port} never came up"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only a directory *we* named is ours to delete. The pid is in the name
    /// so two servers never share a profile, and so "did we create this?" is
    /// decidable from the path alone rather than from a guess about the port.
    #[test]
    fn own_profile_dir_is_pid_and_port_scoped() {
        let a = own_profile_dir(9333);
        let b = own_profile_dir(9334);
        assert_ne!(a, b, "different ports get different profiles");
        assert!(a.starts_with(std::env::temp_dir()));
        let name = a.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(
            name,
            format!("agentctl-cdp-{}-9333", std::process::id()),
            "the pid must be in the name"
        );
    }

    /// An operator-supplied profile is never deleted: `launch_browser` records
    /// `None` for it, and `reap_one` only removes what it is given.
    #[test]
    fn a_supplied_profile_dir_is_not_owned() {
        let spec = json!({ "user_data_dir": "/tmp/somebody-elses-profile", "port": 9999 });
        let supplied = spec.get("user_data_dir").and_then(Value::as_str);
        assert!(supplied.is_some());
        // Mirrors the branch in launch_browser: supplied => not owned.
        let owned: Option<std::path::PathBuf> = match supplied {
            Some(_) => None,
            None => Some(own_profile_dir(9999)),
        };
        assert!(owned.is_none(), "a supplied profile must never be deleted");
    }

    #[tokio::test]
    async fn connect_requires_attach_or_launch() {
        let b = CdpBackend::new(NavPolicy::default());
        let e = b.connect(None, None).await;
        assert!(matches!(e, Err(BrowserError::Failed(_))));
    }

    #[tokio::test]
    async fn tabs_unknown_browser_id_is_not_found() {
        let b = CdpBackend::new(NavPolicy::default());
        let e = b.tabs(999, "list", None, None).await;
        assert!(matches!(e, Err(BrowserError::NotFound(_))));
    }

    #[tokio::test]
    async fn resolve_ws_with_no_browsers_is_not_found() {
        let b = CdpBackend::new(NavPolicy::default());
        assert!(matches!(
            b.resolve_ws("ABC").await,
            Err(BrowserError::NotFound(_))
        ));
    }
}
