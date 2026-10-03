//! Browser engine types, the `BrowserBackend` trait, and the real CDP-backed
//! implementation (`CdpBackend`). Unlike the a11y/input/window engines, the
//! backend is OS-independent (it only speaks TCP/HTTP/WebSocket), so the real
//! implementation lives here rather than in a per-OS crate.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::cdp::{http_json, CdpConn, DialogPolicy, RecordDialogs};
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
    Selector {
        by: &'a str,
        query: &'a str,
        within: Option<&'a str>,
        text: Option<&'a str>,
        index: Option<usize>,
    },
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
    /// [`BrowserBackend::act`] with a flag marking the value as a secret, so
    /// anything that would display the typed value (the showcase HUD) masks
    /// it. Backends that display nothing need not override this.
    async fn act_masked(
        &self,
        target: &str,
        locator: Locator<'_>,
        action: &str,
        value: Option<&str>,
        secret: bool,
    ) -> Result<Value, BrowserError> {
        let _ = secret;
        self.act(target, locator, action, value).await
    }
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
    /// Emulate a viewport for responsive testing (device metrics override).
    /// `width == 0` clears the override and restores the real window size.
    async fn set_viewport(
        &self,
        target: &str,
        width: u32,
        height: u32,
        mobile: bool,
        scale: f64,
    ) -> Result<Value, BrowserError>;
    /// Evaluate arbitrary JS in the page (dangerous).
    async fn eval(&self, target: &str, expression: &str) -> Result<Value, BrowserError>;
    /// Start watching a tab across navigations (the recorder's transport).
    ///
    /// `new_document_script` is registered to run at the start of every new
    /// document in the tab, so it survives reloads and navigations;
    /// `current_document_script` is run once in the page as it is now, and
    /// returns an object with the page's `url`. Both report events by calling
    /// the page function `binding(jsonString)`, which the backend delivers to
    /// the Rust side as they happen. Both run in an isolated world of the tab
    /// (shared DOM, separate JavaScript globals), and the binding exists only
    /// there, so page script can neither call it nor reach the recorder's
    /// state. `dialogs` says who answers the page's JavaScript dialogs while
    /// it is watched; `None` picks `Human` for a visible browser and the
    /// tab's dialog policy (dismiss by default) for a headless one. Needs a
    /// persistent session, so only the CDP engine supports it.
    async fn observe_start(
        &self,
        target: &str,
        binding: &str,
        new_document_script: &str,
        current_document_script: &str,
        dialogs: Option<RecordDialogs>,
    ) -> Result<Value, BrowserError> {
        let _ = (
            target,
            binding,
            new_document_script,
            current_document_script,
            dialogs,
        );
        Err(BrowserError::Unsupported(
            "watching a tab across navigations needs the CDP (Chrome) engine".into(),
        ))
    }
    /// `{recording, event_count, elapsed_ms}` for a tab being watched.
    async fn observe_status(&self, target: &str) -> Result<Value, BrowserError> {
        let _ = target;
        Err(BrowserError::Unsupported(
            "watching a tab across navigations needs the CDP (Chrome) engine".into(),
        ))
    }
    /// Stop watching: run `teardown_script` in the page, unregister the
    /// new-document script, and return `{start_url, events, script_removed}`.
    async fn observe_stop(
        &self,
        target: &str,
        teardown_script: &str,
    ) -> Result<Value, BrowserError> {
        let _ = (target, teardown_script);
        Err(BrowserError::Unsupported(
            "watching a tab across navigations needs the CDP (Chrome) engine".into(),
        ))
    }
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
    /// Fill multiple form fields (and optionally submit) in one round trip.
    async fn fill_form(
        &self,
        target: &str,
        fields: &Value,
        submit: Option<&Value>,
    ) -> Result<Value, BrowserError> {
        let _ = (target, fields, submit);
        Err(BrowserError::Unsupported("fill_form not supported".into()))
    }
    /// Extract structured data using an attribute/CSS schema.
    async fn extract(
        &self,
        target: &str,
        schema: &Value,
        within: Option<&str>,
    ) -> Result<Value, BrowserError> {
        let _ = (target, schema, within);
        Err(BrowserError::Unsupported("extract not supported".into()))
    }
    /// Capture raw cookies and storage for profile saving.
    async fn profile_state(&self, target: &str) -> Result<Value, BrowserError> {
        let _ = target;
        Err(BrowserError::Unsupported(
            "profile_state not supported".into(),
        ))
    }
    /// Restore cookies and storage into target tab.
    async fn profile_restore(&self, target: &str, state: &Value) -> Result<Value, BrowserError> {
        let _ = (target, state);
        Err(BrowserError::Unsupported(
            "profile_restore not supported".into(),
        ))
    }
    /// Create an isolated speculative browser branch from a target tab.
    async fn branch_create(&self, target_id: &str, branch_id: &str) -> Result<Value, BrowserError> {
        let _ = (target_id, branch_id);
        Err(BrowserError::Unsupported(
            "branch_create not supported".into(),
        ))
    }
    /// Commit a speculative branch back to its parent tab.
    async fn branch_commit(&self, branch_id: &str) -> Result<Value, BrowserError> {
        let _ = branch_id;
        Err(BrowserError::Unsupported(
            "branch_commit not supported".into(),
        ))
    }
    /// Discard a speculative branch and reap all its allocated contexts/tabs.
    async fn branch_discard(&self, branch_id: &str) -> Result<Value, BrowserError> {
        let _ = branch_id;
        Err(BrowserError::Unsupported(
            "branch_discard not supported".into(),
        ))
    }
    /// Switch focus/activation to a branch's tab.
    async fn branch_switch(&self, branch_id: &str) -> Result<Value, BrowserError> {
        let _ = branch_id;
        Err(BrowserError::Unsupported(
            "branch_switch not supported".into(),
        ))
    }
    /// List all active or recorded speculative branches.
    async fn branch_list(&self, target_id: Option<&str>) -> Result<Value, BrowserError> {
        let _ = target_id;
        Err(BrowserError::Unsupported(
            "branch_list not supported".into(),
        ))
    }
    /// Save an in-memory checkpoint (a deep copy of form state, storage and cookies) of a tab.
    async fn checkpoint_save(
        &self,
        target_id: &str,
        tag: Option<&str>,
    ) -> Result<Value, BrowserError> {
        let _ = (target_id, tag);
        Err(BrowserError::Unsupported(
            "checkpoint_save not supported".into(),
        ))
    }
    /// Rollback a tab to a saved checkpoint (T-1).
    async fn checkpoint_rollback(
        &self,
        target_id: &str,
        tag: Option<&str>,
    ) -> Result<Value, BrowserError> {
        let _ = (target_id, tag);
        Err(BrowserError::Unsupported(
            "checkpoint_rollback not supported".into(),
        ))
    }
    /// List available checkpoints for a target tab.
    async fn checkpoint_list(&self, target_id: Option<&str>) -> Result<Value, BrowserError> {
        let _ = target_id;
        Err(BrowserError::Unsupported(
            "checkpoint_list not supported".into(),
        ))
    }
    /// Delete checkpoints for a target tab.
    async fn checkpoint_delete(
        &self,
        target_id: &str,
        tag: Option<&str>,
    ) -> Result<Value, BrowserError> {
        let _ = (target_id, tag);
        Err(BrowserError::Unsupported(
            "checkpoint_delete not supported".into(),
        ))
    }
    /// Configure or query showcase visual flair (animated virtual cursor, click ripple, typing HUD).
    async fn showcase(
        &self,
        target: &str,
        config: Option<crate::showcase::ShowcaseConfig>,
    ) -> Result<Value, BrowserError> {
        let _ = (target, config);
        Err(BrowserError::Unsupported("showcase not supported".into()))
    }
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
    /// Where the browser's own CDP endpoint answers, so it can be asked to quit
    /// itself. Killing the launcher process is not enough for a sandboxed
    /// (flatpak/snap) Chrome: it reparents its real processes out of our
    /// process group, so only `Browser.close` over CDP tears the whole tree
    /// down. Always loopback, but kept explicit alongside the port.
    host: String,
    port: u16,
}

struct SafariEntry {
    proc: Mutex<Option<crate::safari::SafariProcess>>,
    session: crate::safari::SafariSession,
}

/// The real Chrome DevTools Protocol backend.
pub struct CdpBackend {
    browsers: Mutex<Vec<BrowserEntry>>,
    /// Browsers started by this process, by `browser_id`.
    launched: Mutex<Vec<Launched>>,
    safari_sessions: Mutex<HashMap<u32, std::sync::Arc<SafariEntry>>>,
    next_id: AtomicU32,
    /// Where `goto` may take the browser: `browser.allowed_origins` plus the
    /// resolved-address check (see [`crate::nav`]).
    nav: NavPolicy,
    /// Per-target answer for JavaScript dialogs, and the log of ones answered.
    /// Connections are per-call, so the policy has to live with the backend.
    dialogs: Mutex<HashMap<String, (DialogPolicy, Vec<Value>)>>,
    /// In-memory manager for speculative browser branches.
    branches: Mutex<crate::branch::BranchManager>,
    /// In-memory checkpoint store.
    checkpoints: Mutex<crate::checkpoint::CheckpointStore>,
    /// Configured showcase visual flair (animated cursor, click ripples, typing HUD).
    showcase: Mutex<crate::showcase::ShowcaseConfig>,
    /// Per target: the document-identity marker planted on the document an
    /// action (goto, reload, click, submit, press) was about to leave. A
    /// `wait navigation` that finds one waits for a document without it.
    nav_pending: Mutex<HashMap<String, NavPending>>,
    /// Tabs being watched across navigations (the recorder), by target id.
    observers: Mutex<HashMap<String, Observer>>,
}

fn poisoned() -> BrowserError {
    BrowserError::Failed("internal state lock poisoned".into())
}

/// Most events kept for one watched tab; a page that fires input events in a
/// loop must not grow this without bound.
const MAX_OBSERVED_EVENTS: usize = 20_000;

/// A tab being watched by [`BrowserBackend::observe_start`]. A task owns the
/// session: scripts registered for new documents and the page-to-Rust binding
/// belong to the session that created them, so the session has to stay open
/// for as long as the tab is watched.
struct Observer {
    events: std::sync::Arc<Mutex<Vec<Value>>>,
    /// Ask the task to tear down (the script to run in the page, and where to
    /// report whether the new-document script was removed).
    stop: tokio::sync::oneshot::Sender<(String, tokio::sync::oneshot::Sender<ObserverDone>)>,
    start_url: String,
    started: std::time::Instant,
    task: tokio::task::JoinHandle<()>,
}

/// Name of the isolated world the recorder and its binding live in. Isolated
/// worlds share the DOM (listeners see the user's clicks and typing) but have
/// their own JavaScript globals, so page script cannot call the binding or
/// touch the recorder's state.
const RECORDER_WORLD: &str = "agentctl_recorder";

/// Keep the set of execution contexts that belong to the recorder's world
/// current from `Runtime.executionContext*` events, so the teardown can run in
/// the very context the recorder lives in.
fn track_world(v: &Value, worlds: &mut std::collections::HashSet<i64>) {
    let params = v.get("params");
    match v.get("method").and_then(Value::as_str) {
        Some("Runtime.executionContextCreated") => {
            let ctx = params.and_then(|p| p.get("context"));
            let named = ctx.and_then(|c| c.get("name")).and_then(Value::as_str);
            if named == Some(RECORDER_WORLD) {
                if let Some(id) = ctx.and_then(|c| c.get("id")).and_then(Value::as_i64) {
                    worlds.insert(id);
                }
            }
        }
        Some("Runtime.executionContextDestroyed") => {
            if let Some(id) = params
                .and_then(|p| p.get("executionContextId"))
                .and_then(Value::as_i64)
            {
                worlds.remove(&id);
            }
        }
        Some("Runtime.executionContextsCleared") => worlds.clear(),
        _ => {}
    }
}

/// What the session task reports when it has torn down.
struct ObserverDone {
    /// The new-document script was unregistered.
    script_removed: bool,
    /// JavaScript dialogs the session answered while it was attached.
    dialogs: Vec<Value>,
}

/// What the session task tracks about the tab it watches.
struct Watch {
    binding: String,
    events: std::sync::Arc<Mutex<Vec<Value>>>,
    started: std::time::Instant,
    /// Execution contexts of the recorder's isolated world.
    worlds: std::collections::HashSet<i64>,
    /// A person answers the page's dialogs (nobody else does).
    human: bool,
    /// The dialog on screen right now: `(type, message, url)`.
    open_dialog: Option<(String, String, String)>,
}

impl Watch {
    /// A dialog is waiting for a person, so the page's thread is blocked and
    /// nothing can be evaluated in it until they answer.
    fn blocked_on_person(&self) -> bool {
        self.human && self.open_dialog.is_some()
    }

    fn push_event(&self, mut ev: Value) {
        if let Some(map) = ev.as_object_mut() {
            map.insert(
                "timestamp_ms".into(),
                json!(self.started.elapsed().as_millis() as u64),
            );
            if let Ok(mut v) = self.events.lock() {
                if v.len() < MAX_OBSERVED_EVENTS {
                    v.push(ev);
                }
            }
        }
    }
}

/// One message from a watched page. A dialog it raises is answered with the
/// recording's dialog setting, or left to the person at the window; either way
/// how it ended becomes a `dialog` event, so the flow can reproduce it.
/// (Attaching with the Page domain, which registering a new-document script
/// requires, makes this session one Chrome announces dialogs to.)
async fn observe_message(c: &mut CdpConn, v: &Value, w: &mut Watch) {
    track_world(v, &mut w.worlds);
    let params = v.get("params").cloned().unwrap_or_else(|| json!({}));
    let text = |k: &str| {
        params
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    match v.get("method").and_then(Value::as_str) {
        Some("Page.javascriptDialogOpening") => {
            w.open_dialog = Some((text("type"), text("message"), text("url")));
            if !w.human {
                let _ = c.answer_dialog(&params).await;
            }
        }
        Some("Page.javascriptDialogClosed") => {
            let Some((kind, message, url)) = w.open_dialog.take() else {
                return;
            };
            let accepted = params.get("result").and_then(Value::as_bool) == Some(true);
            let policy = if accepted { "accept" } else { "dismiss" };
            if w.human {
                c.note_dialog(json!({
                    "type": kind, "message": message, "url": url,
                    "answered": if accepted { "accepted" } else { "dismissed" },
                    "by": "person",
                }));
            }
            // The typed answer of a prompt is not kept: it may be a secret.
            w.push_event(json!({
                "kind": "dialog", "tag": "", "selector": "",
                "text": kind, "value": policy, "url": url,
            }));
        }
        _ => record_observed(v, &w.binding, &w.events, w.started),
    }
}

/// Keep a `binding(jsonString)` call from the page as an event, stamped with
/// the time since the watch began. Anything else on the wire is ignored.
fn record_observed(
    msg: &Value,
    binding: &str,
    events: &Mutex<Vec<Value>>,
    started: std::time::Instant,
) {
    if msg.get("method").and_then(Value::as_str) != Some("Runtime.bindingCalled") {
        return;
    }
    let Some(params) = msg.get("params") else {
        return;
    };
    if params.get("name").and_then(Value::as_str) != Some(binding) {
        return;
    }
    let Some(mut ev) = params
        .get("payload")
        .and_then(Value::as_str)
        .and_then(|p| serde_json::from_str::<Value>(p).ok())
    else {
        return;
    };
    if let Some(map) = ev.as_object_mut() {
        map.insert(
            "timestamp_ms".into(),
            json!(started.elapsed().as_millis() as u64),
        );
        if let Ok(mut v) = events.lock() {
            if v.len() < MAX_OBSERVED_EVENTS {
                v.push(ev);
            }
        }
    }
}

/// The session task behind an [`Observer`]: read the page's events until asked
/// to stop, then run the teardown, drain what is still in flight, and remove
/// the new-document script. Reads are cancel-safe, so being interrupted by the
/// stop request never loses part of a message.
async fn observe_session(
    mut c: CdpConn,
    mut w: Watch,
    script_id: Option<String>,
    early: Vec<Value>,
    mut stop_rx: tokio::sync::oneshot::Receiver<(
        String,
        tokio::sync::oneshot::Sender<ObserverDone>,
    )>,
) {
    for v in early {
        observe_message(&mut c, &v, &mut w).await;
    }
    let (teardown, reply) = loop {
        tokio::select! {
            req = &mut stop_rx => match req {
                Ok(r) => break r,
                // The backend dropped the handle: nobody is listening.
                Err(_) => return,
            },
            msg = c.read_message() => match msg {
                Ok(v) => observe_message(&mut c, &v, &mut w).await,
                // The tab or browser went away; what was captured stays.
                Err(_) => return,
            },
        }
    };
    c.keep_events(true);
    // A dialog still waiting for the person blocks the page: evaluating or
    // removing a script would only hang. Closing this session drops the
    // new-document script with it, and the dialog stays on screen to answer.
    let blocked = w.blocked_on_person();
    if !blocked {
        // The recorder lives in its isolated world(s), not the main world.
        for id in w.worlds.clone() {
            let _ = c
                .call(
                    "Runtime.evaluate",
                    json!({ "expression": teardown, "contextId": id, "returnByValue": true }),
                )
                .await;
        }
    }
    for v in c.take_events() {
        observe_message(&mut c, &v, &mut w).await;
    }
    // Calls made just before the teardown may still be on their way.
    while let Ok(Ok(v)) =
        tokio::time::timeout(tokio::time::Duration::from_millis(150), c.read_message()).await
    {
        observe_message(&mut c, &v, &mut w).await;
    }
    let script_removed = match script_id {
        Some(id) if !blocked => c
            .call(
                "Page.removeScriptToEvaluateOnNewDocument",
                json!({ "identifier": id }),
            )
            .await
            .is_ok(),
        _ => false,
    };
    let _ = reply.send(ObserverDone {
        script_removed,
        dialogs: c.take_dialogs(),
    });
}

/// How long a `wait navigation` keeps expecting the navigation that a click,
/// submit or key press may have started but that has not begun yet (a handler
/// that defers `location` with a timer). After this the click is taken to
/// have navigated nowhere and the wait settles on the loaded page, reporting
/// `navigated: false`. Not applied to goto/reload, which always navigate.
const NAV_EXPECT_MS: u64 = 2_000;

/// A document-identity marker waiting to be left behind. See
/// [`CdpBackend::nav_pending`].
#[derive(Debug, Clone)]
struct NavPending {
    token: String,
    set_at: std::time::Instant,
    /// A navigation is known to be under way (goto/reload with a new
    /// document), so the wait never gives up on it. A click only might.
    certain: bool,
}

/// A fresh marker value, unique within the process.
fn new_nav_token() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{n}-{t}")
}

/// What a `wait navigation` should do with a probe of the page: `Some(true)` the
/// document was replaced and has loaded, `Some(false)` give up because nothing
/// began navigating (click only, past its grace window), `None` keep waiting.
/// Pure so the decision is tested without a browser.
fn nav_probe_verdict(
    marker: Option<&str>,
    ready_state: &str,
    pending_token: &str,
    certain: bool,
    since_set: std::time::Duration,
) -> Option<bool> {
    if ready_state != "complete" {
        return None;
    }
    if marker != Some(pending_token) {
        return Some(true);
    }
    if !certain && since_set >= std::time::Duration::from_millis(NAV_EXPECT_MS) {
        return Some(false);
    }
    None
}

impl CdpBackend {
    pub fn new(nav: NavPolicy) -> Self {
        CdpBackend {
            browsers: Mutex::new(Vec::new()),
            launched: Mutex::new(Vec::new()),
            safari_sessions: Mutex::new(HashMap::new()),
            next_id: AtomicU32::new(1),
            nav,
            dialogs: Mutex::new(HashMap::new()),
            branches: Mutex::new(crate::branch::BranchManager::with_max_active(
                std::env::var("AGENTCTL_MAX_BRANCHES")
                    .ok()
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .filter(|n| *n > 0)
                    .unwrap_or(crate::branch::BranchManager::DEFAULT_MAX_ACTIVE),
            )),
            checkpoints: Mutex::new(crate::checkpoint::CheckpointStore::new()),
            showcase: Mutex::new(crate::showcase::ShowcaseConfig::default()),
            nav_pending: Mutex::new(HashMap::new()),
            observers: Mutex::new(HashMap::new()),
        }
    }

    /// Plant a fresh marker on the document `c` is attached to and remember it
    /// as the one `wait navigation` should expect to see replaced. Best effort:
    /// a page that cannot be scripted just gets no marker (and no pending).
    async fn plant_nav_token(&self, c: &mut CdpConn, target: &str, certain: bool) {
        let token = new_nav_token();
        let js = format!("window.__agentctl_nav_token = {token:?}; true");
        if Self::eval_value(c, &js).await.is_ok() {
            self.set_nav_pending(target, token, certain);
        }
    }

    fn set_nav_pending(&self, target: &str, token: String, certain: bool) {
        if let Ok(mut m) = self.nav_pending.lock() {
            m.insert(
                target.to_string(),
                NavPending {
                    token,
                    set_at: std::time::Instant::now(),
                    certain,
                },
            );
        }
    }

    fn clear_nav_pending(&self, target: &str, only_token: Option<&str>) {
        if let Ok(mut m) = self.nav_pending.lock() {
            let matches = match only_token {
                None => true,
                Some(t) => m.get(target).is_some_and(|p| p.token == t),
            };
            if matches {
                m.remove(target);
            }
        }
    }

    fn nav_pending_for(&self, target: &str) -> Option<NavPending> {
        self.nav_pending.lock().ok()?.get(target).cloned()
    }

    /// `wait navigation` with a marker pending: wait for a loaded document
    /// that is not the one the marker was planted on. The probe can fail while
    /// the old execution context is torn down; that just means "not yet".
    async fn wait_replaced_document(
        &self,
        target: &str,
        c: &mut CdpConn,
        pending: NavPending,
        timeout_ms: u64,
    ) -> Result<Value, BrowserError> {
        let deadline = tokio::time::Instant::now()
            + tokio::time::Duration::from_millis(timeout_ms.clamp(50, 60_000));
        loop {
            if let Ok(v) = Self::eval_value(
                c,
                "({tok: window.__agentctl_nav_token === undefined ? null : String(window.__agentctl_nav_token), state: document.readyState})",
            )
            .await
            {
                let verdict = nav_probe_verdict(
                    v.get("tok").and_then(Value::as_str),
                    v.get("state").and_then(Value::as_str).unwrap_or(""),
                    &pending.token,
                    pending.certain,
                    pending.set_at.elapsed(),
                );
                if let Some(navigated) = verdict {
                    self.clear_nav_pending(target, Some(&pending.token));
                    let mut out =
                        json!({ "settled": true, "condition": "navigation", "navigated": navigated });
                    self.note_dialogs(target, c, &mut out);
                    return Ok(out);
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(BrowserError::Timeout(format!(
                    "wait 'navigation' did not settle in {timeout_ms}ms: the page never replaced the document it was on"
                )));
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }
    }

    /// Cap the number of simultaneously active speculative branches
    /// (default 8, or `AGENTCTL_MAX_BRANCHES`).
    pub fn with_max_branches(mut self, max: usize) -> Self {
        self.branches = Mutex::new(crate::branch::BranchManager::with_max_active(max.max(1)));
        self
    }

    /// Attach showcase configuration for demo/presentation flair.
    pub fn with_showcase(mut self, config: crate::showcase::ShowcaseConfig) -> Self {
        self.showcase = Mutex::new(config);
        self
    }

    fn get_safari_session(
        &self,
        target: &str,
    ) -> Result<std::sync::Arc<SafariEntry>, BrowserError> {
        let prefix = "safari-";
        if let Some(rest) = target.strip_prefix(prefix) {
            let id = rest
                .parse::<u32>()
                .map_err(|_| BrowserError::NotFound(format!("invalid safari target '{target}'")))?;
            let guard = self
                .safari_sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            guard.get(&id).cloned().ok_or_else(|| {
                BrowserError::NotFound(format!("safari session '{target}' not found"))
            })
        } else {
            Err(BrowserError::NotFound(format!(
                "target '{target}' is not a safari target"
            )))
        }
    }

    /// Stop every browser this process started and remove the profiles it
    /// created. Idempotent, so `Drop` and an explicit shutdown can both run.
    fn reap_all(&self) {
        // Close the branches' real tabs and contexts, not just flip their
        // status: an attached browser outlives us and would keep them forever.
        let active = match self.branches.lock() {
            Ok(mut g) => {
                let active = g.active_branches();
                for b in &active {
                    let _ = g.mark_discarded(&b.branch_id);
                }
                active
            }
            Err(_) => Vec::new(),
        };
        for (id, e) in teardown_branches_blocking(active) {
            tracing::warn!(branch = %id, "could not close branch on shutdown: {}", err_msg(&e));
        }
        let taken: Vec<Launched> = {
            let mut g = self.launched.lock().expect("launched mutex");
            std::mem::take(&mut *g)
        };
        for l in taken {
            tracing::info!(
                browser_id = l.id,
                "stopping browser launched by this session"
            );
            // Ask the browser to quit itself first (reaches a sandboxed tree a
            // signal cannot), then reap the launcher and delete the profile.
            browser_close_blocking(&l.host, l.port);
            reap_one(l.child, l.user_data_dir.as_deref());
        }
        let mut safari_guard = self
            .safari_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for (_, entry) in safari_guard.drain() {
            if let Ok(mut proc_opt) = entry.proc.lock() {
                if let Some(mut p) = proc_opt.take() {
                    p.kill();
                }
            }
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

    /// Find the owning browser and its top-level browser `webSocketDebuggerUrl` for a given target.
    async fn browser_ws_for_target(
        &self,
        target: &str,
    ) -> Result<(BrowserEntry, String), BrowserError> {
        for b in self.browsers() {
            let list = match http_json(&b.host, b.port, "GET", "/json/list").await {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(arr) = list.as_array() {
                for t in arr {
                    if t.get("id").and_then(Value::as_str) == Some(target) {
                        let ver = http_json(&b.host, b.port, "GET", "/json/version").await?;
                        let ws = ver
                            .get("webSocketDebuggerUrl")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                BrowserError::Failed("no browser webSocketDebuggerUrl".into())
                            })?;
                        return Ok((b, ws.to_string()));
                    }
                }
            }
        }
        Err(BrowserError::NotFound(format!(
            "target '{target}' not found in any connected browser"
        )))
    }

    /// Whether the browser that owns `target` has no window (the user agent
    /// says `HeadlessChrome`). When that cannot be told, assume headless: the
    /// answer decides whether a person is there to answer a dialog, and a
    /// wrong "yes" hangs the tab.
    async fn target_is_headless(&self, target: &str) -> bool {
        let Ok((b, _)) = self.browser_ws_for_target(target).await else {
            return true;
        };
        match http_json(&b.host, b.port, "GET", "/json/version").await {
            Ok(v) => v
                .get("User-Agent")
                .and_then(Value::as_str)
                .map_or(true, |ua| ua.contains("Headless")),
            Err(_) => true,
        }
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

    /// Remove the showcase overlay from every tab of one browser (or of all
    /// of them). Best effort: a tab that has gone away has nothing to clean.
    async fn teardown_showcase(&self, only_browser: Option<u32>) {
        let mut ids: Vec<u32> = self.browsers().into_iter().map(|b| b.id).collect();
        ids.extend(
            self.safari_sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .keys()
                .copied(),
        );
        let script = crate::showcase::JS_SHOWCASE_TEARDOWN;
        for id in ids {
            if only_browser.is_some_and(|b| b != id) {
                continue;
            }
            let Ok(list) = self.tabs(id, "list", None, None).await else {
                continue;
            };
            let targets: Vec<String> = list
                .get("tabs")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|t| t.get("target_id").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            for t in targets {
                if t.starts_with("safari-") {
                    if let Ok(entry) = self.get_safari_session(&t) {
                        let _ = entry
                            .session
                            .execute_sync(&format!("return {script};"), &[])
                            .await;
                    }
                } else if let Ok(mut c) = self.conn(&t).await {
                    let _ = Self::eval_value(&mut c, script).await;
                }
            }
        }
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

    /// Real pointer input at a viewport CSS-pixel point: a `mouseMoved`, and
    /// for `click` a left `mousePressed` + `mouseReleased` (clickCount 1).
    /// These are trusted events (`isTrusted === true`) in the page.
    async fn cdp_mouse(c: &mut CdpConn, x: f64, y: f64, click: bool) -> Result<(), BrowserError> {
        c.call(
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseMoved", "x": x, "y": y, "button": "none", "buttons": 0 }),
        )
        .await?;
        if click {
            c.call(
                "Input.dispatchMouseEvent",
                json!({ "type": "mousePressed", "x": x, "y": y, "button": "left", "buttons": 1, "clickCount": 1 }),
            )
            .await?;
            c.call(
                "Input.dispatchMouseEvent",
                json!({ "type": "mouseReleased", "x": x, "y": y, "button": "left", "buttons": 0, "clickCount": 1 }),
            )
            .await?;
        }
        Ok(())
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

/// CDP key-event parameters for a named key.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct KeySpec {
    pub key: &'static str,
    pub code: &'static str,
    pub vk: u32,
    pub text: Option<&'static str>,
}

/// Map a key name to its CDP key-event parameters; `None` for unsupported keys.
pub(crate) fn key_event_spec(name: &str) -> Option<KeySpec> {
    match name {
        "Enter" => Some(KeySpec {
            key: "Enter",
            code: "Enter",
            vk: 13,
            text: Some("\r"),
        }),
        "Escape" => Some(KeySpec {
            key: "Escape",
            code: "Escape",
            vk: 27,
            text: None,
        }),
        "Tab" => Some(KeySpec {
            key: "Tab",
            code: "Tab",
            vk: 9,
            text: None,
        }),
        _ => None,
    }
}

impl CdpBackend {
    /// Navigate `target` to `url` and block until the *new* document has
    /// finished loading. A bare `readyState` poll is not enough: right after
    /// `Page.navigate` the old document still reports `complete`, so a marker
    /// is planted on it and the wait is for a document without that marker.
    /// Errors on a denied URL, a navigation error (`errorText`) or a timeout.
    async fn goto_and_wait(
        &self,
        target: &str,
        url: &str,
        timeout_ms: u64,
    ) -> Result<(), BrowserError> {
        if let Err(denied) = self.nav.check(url).await {
            return Err(BrowserError::PermissionDenied(denied.message()));
        }
        let mut c = self.conn(target).await?;
        // Best effort: a page that cannot be scripted just loses the marker.
        Self::eval_value(&mut c, "window.__agentctl_nav_mark = true; true")
            .await
            .ok();
        let r = c.call("Page.navigate", json!({ "url": url })).await?;
        if let Some(err) = r.get("errorText").and_then(Value::as_str) {
            return Err(BrowserError::Failed(format!("navigate to {url}: {err}")));
        }
        // A same-document navigation (fragment change) has no loaderId and
        // keeps the old document, marker included.
        let new_document = r.get("loaderId").is_some();
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(timeout_ms);
        loop {
            // The execution context disappears mid-navigation; just retry.
            if let Ok(v) = Self::eval_value(
                &mut c,
                "({old: window.__agentctl_nav_mark === true, state: document.readyState})",
            )
            .await
            {
                let old = v.get("old").and_then(Value::as_bool).unwrap_or(false);
                let complete = v.get("state").and_then(Value::as_str) == Some("complete");
                if complete && (!new_document || !old) {
                    return Ok(());
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(BrowserError::Timeout(format!(
                    "{url} did not finish loading in {timeout_ms}ms"
                )));
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }
    }
}

/// JS helper: XPath of an element (id-anchored when possible, supporting shadow DOM and canvas regions).
const JS_XPATH: &str = r#"
function __xp(el){
  if(!el) return '';
  var segments = [];
  var curr = el;
  while(curr && curr.nodeType === 1){
    var root = curr.getRootNode ? curr.getRootNode() : null;
    var inShadow = !!(root && root.host);
    if(curr.id && !inShadow){
      segments.unshift('//*[@id="'+curr.id+'"]');
      break;
    }
    var parts = [];
    var node = curr;
    while(node && node.nodeType === 1 && node.tagName !== 'HTML'){
      var ix = 1, sib = node.previousElementSibling;
      while(sib){ if(sib.tagName === node.tagName) ix++; sib = sib.previousElementSibling; }
      parts.unshift(node.tagName.toLowerCase() + '[' + ix + ']');
      var p = node.parentElement;
      if(!p && node.parentNode && node.parentNode.host){
        break;
      }
      node = p;
    }
    var seg = (inShadow ? '' : '/html/') + parts.join('/');
    segments.unshift(seg);
    curr = inShadow ? root.host : null;
  }
  return segments.join('::shadow/');
}

// Page-published canvas regions: a canvas only has child nodes when the page
// itself declares them via `canvas.__agentctl_regions` or a JSON
// `data-canvas-regions` attribute. Nothing is inferred from pixels.
// Region x/y/w/h are in canvas bitmap pixels (the same space as ctx drawing
// calls); __canvas_box maps them to viewport CSS pixels.
function __canvas_regions(canvas){
  var r = canvas.__agentctl_regions;
  if(Array.isArray(r) && r.length > 0) return r;
  try {
    var d = JSON.parse(canvas.getAttribute('data-canvas-regions') || '[]');
    if(Array.isArray(d)) return d;
  } catch(e){}
  return [];
}
// The id a region is addressed by in refs. Used by both snapshot and resolve
// so the two always agree on which region a ref names.
function __canvas_reg_id(reg, i){
  var v = reg.id || reg.label || reg.text;
  return v ? String(v) : ('reg_' + i);
}
// Escape only what would break `::canvas[<id>]` parsing (and '%' itself).
function __canvas_enc(id){
  return id.replace(/[%\[\]:"'\\]/g, function(c){
    return '%' + ('0' + c.charCodeAt(0).toString(16).toUpperCase()).slice(-2);
  });
}
// Region box in viewport CSS pixels. Canvas content box (inside border and
// padding) is mapped from bitmap space, so a CSS-scaled canvas is handled.
function __canvas_box(canvas, reg){
  var rect = canvas.getBoundingClientRect();
  var left = rect.left + canvas.clientLeft;
  var top = rect.top + canvas.clientTop;
  var sx = canvas.width > 0 ? canvas.clientWidth / canvas.width : 1;
  var sy = canvas.height > 0 ? canvas.clientHeight / canvas.height : 1;
  var w = (reg.w || 40) * sx, h = (reg.h || 40) * sy;
  return { x: left + (reg.x || 0) * sx, y: top + (reg.y || 0) * sy, w: w, h: h };
}
function __resolve(xp){
  if(!xp) return null;
  if(xp.indexOf('::canvas[') >= 0){
    var cat = xp.lastIndexOf('::canvas[');
    if(xp.charAt(xp.length - 1) !== ']') return null;
    var canvasXp = xp.slice(0, cat);
    var btnKey;
    try { btnKey = decodeURIComponent(xp.slice(cat + 9, -1)); } catch(e){ return null; }
    var canvas = __resolve(canvasXp);
    if(!canvas) return null;
    var regions = __canvas_regions(canvas);
    for(var k = 0; k < regions.length; k++){
      if(__canvas_reg_id(regions[k], k) === btnKey){
        return { __is_canvas_target: true, canvas: canvas, reg: regions[k] };
      }
    }
    return null;
  }
  if(xp.indexOf('::shadow/') >= 0){
    var parts = xp.split('::shadow/');
    var curr = document;
    for(var i = 0; i < parts.length; i++){
      var seg = parts[i];
      if(i === 0){
        var r = document.evaluate(seg, document, null, XPathResult.FIRST_ORDERED_NODE_TYPE, null);
        var host = r.singleNodeValue;
        if(!host) return null;
        if(!host.shadowRoot) return null;
        curr = host.shadowRoot;
      } else {
        var found = null;
        try {
          var selector = seg.replace(/\[(\d+)\]/g, ':nth-of-type($1)').replace(/\//g, ' > ');
          if(selector.startsWith(' > ')) selector = selector.slice(3);
          found = curr.querySelector(selector);
        } catch(e){}
        if(!found){
          try {
            var r = document.evaluate('.//' + seg, curr, null, XPathResult.FIRST_ORDERED_NODE_TYPE, null);
            found = r.singleNodeValue;
          } catch(e){}
        }
        if(!found) return null;
        if(i === parts.length - 1) return found;
        if(!found.shadowRoot) return null;
        curr = found.shadowRoot;
      }
    }
    return null;
  }
  var r = document.evaluate(xp, document, null, XPathResult.FIRST_ORDERED_NODE_TYPE, null);
  return r.singleNodeValue;
}
"#;

/// Resolve an element by selector, matching `browser_query`'s `by` values, for
/// act-by-selector. Supports scoped container root (`within`), substring text filter (`textFilter`),
/// and ordinal selection (`index`). Searches across open shadow roots.
const JS_FIND: &str = r#"
function __find(by, q, within, textFilter, index){
  var root = document;
  if(within) {
    // Decide XPath vs CSS first: an XPath string is a CSS syntax error, so
    // trying querySelector first threw before the XPath branch was reached.
    var isXp = within.charAt(0) === '/' || within.charAt(0) === '(';
    var w = null;
    try {
      if(isXp) {
        w = document.evaluate(within, document, null, XPathResult.FIRST_ORDERED_NODE_TYPE, null).singleNodeValue;
      } else {
        w = document.querySelector(within);
      }
    } catch(e) {
      throw new Error("invalid 'within' " + (isXp ? 'xpath' : 'selector') + ' ' + within + ': ' + (e && e.message ? e.message : e));
    }
    // Never widen: a missing scope root is an error, not "search everything".
    if(!w) throw new Error("'within' root not found: " + within);
    if(!w.querySelectorAll) throw new Error("'within' root is not an element: " + within);
    root = w;
  }
  var matches = [];
  if(by==='css') {
    var els = root.querySelectorAll ? root.querySelectorAll(q) : [];
    for(var i=0; i<els.length; i++) matches.push(els[i]);
    if(matches.length === 0) {
      (function walk(r){
        if(!r) return;
        var children = r.querySelectorAll ? r.querySelectorAll('*') : [];
        for(var i=0; i<children.length; i++){
          try {
            if(children[i].matches && children[i].matches(q)) matches.push(children[i]);
          } catch(e){}
          if(children[i].shadowRoot) walk(children[i].shadowRoot);
        }
      })(root);
    }
  } else if(by==='xpath'){
    var xq = q;
    if(root !== document) {
      // An absolute path ignores the context node and would search the whole
      // document, i.e. silently escape `within`.
      var t = xq.replace(/^\s+/, '');
      if(t.indexOf('//') === 0) {
        xq = '.' + t;
      } else if(t.charAt(0) === '/' || /^\(+\s*\//.test(t)) {
        throw new Error("by=xpath with 'within' needs a relative query (e.g. './/button'); absolute XPath would search the whole document: " + q);
      }
    }
    try {
      var r = document.evaluate(xq, root, null, XPathResult.ORDERED_NODE_SNAPSHOT_TYPE, null);
      for(var i=0; i<r.snapshotLength; i++) matches.push(r.snapshotItem(i));
    } catch(e){}
  } else { // text
    var wAll = root.querySelectorAll ? root.querySelectorAll('*') : [];
    for(var i=0; i<wAll.length; i++){
      if(wAll[i].children.length===0 && (wAll[i].innerText||'').indexOf(q)>=0) matches.push(wAll[i]);
    }
    for(var j=0; j<wAll.length; j++){
      if((wAll[j].textContent||'').trim()===q && matches.indexOf(wAll[j])===-1) matches.push(wAll[j]);
    }
  }

  if(textFilter && typeof textFilter === 'string' && textFilter.length > 0) {
    var tf = textFilter.toLowerCase();
    matches = matches.filter(function(el){
      var t = (el.innerText || el.textContent || el.value || '').trim().toLowerCase();
      return t.indexOf(tf) >= 0;
    });
  }

  if(matches.length === 0) return null;
  var idx = (typeof index === 'number' && index >= 0) ? index : 0;
  return matches[idx] || null;
}
"#;

/// Check whether HTMX has finished all in-flight requests and DOM swaps.
///
/// `htmx` has no "is anything in flight" API, so the first probe installs
/// (idempotently) capture-phase listeners on `document` for
/// `htmx:beforeRequest` / `htmx:afterRequest` / `htmx:afterSettle`, keeping an
/// in-flight counter and the time of the last event. Settled means: counter 0,
/// no element carrying `htmx-request` / `htmx-settling` / `htmx-swapping`
/// (this also covers requests that started before the probe was installed),
/// and no htmx event for a short quiet window. Throws when `window.htmx` is
/// absent so a page without htmx is an error, not "settled".
const JS_HTMX_SETTLED: &str = r#"(function(){
  if (!window.htmx) throw new Error('htmx not present on page');
  var st = window.__agentctl_htmx;
  if (!st) {
    st = window.__agentctl_htmx = { inflight: 0, last: Date.now() };
    var touch = function(){ st.last = Date.now(); };
    document.addEventListener('htmx:beforeRequest', function(){ st.inflight++; touch(); }, true);
    document.addEventListener('htmx:afterRequest', function(){ if (st.inflight > 0) st.inflight--; touch(); }, true);
    document.addEventListener('htmx:afterSettle', touch, true);
  }
  if (st.inflight > 0) return false;
  if (document.querySelector('.htmx-request, .htmx-settling, .htmx-swapping') !== null) return false;
  if (Date.now() - st.last < 100) return false;
  return document.readyState === 'complete' || document.readyState === 'interactive';
})()"#;

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

/// The assertion engine, evaluated in the page. `__SPEC__` is replaced with the
/// JSON spec before evaluation (a raw string, so no brace-escaping). It runs
/// every clause the spec asks for and returns `{checks:[{name,ok,detail,...}]}`.
/// Clauses: text/not_text/url/selector (+min_count), no_console_errors,
/// no_failed_requests, a11y (built-in WCAG rules), style (design-token
/// conformance), component (state assertions), all optionally scoped to
/// `within` (a component root selector).
const JS_ASSERT: &str = r##"(function(){
  var spec=__SPEC__, checks=[], A=window.__agentctl;
  var root=document;
  if(spec.within!=null){ root=document.querySelector(spec.within); if(!root){ checks.push({name:'within',ok:false,detail:'root not found: '+spec.within}); return {checks:checks}; } }
  var scopeText = (root===document ? (document.body?document.body.innerText:'') : (root.innerText||''));
  function visible(el){ if(!el||el.nodeType!==1) return false; var s=getComputedStyle(el); if(s.display==='none'||s.visibility==='hidden'||parseFloat(s.opacity)===0) return false; var r=el.getBoundingClientRect(); return r.width>0&&r.height>0; }
  function pc(c){ var m=/rgba?\(([\d.]+),\s*([\d.]+),\s*([\d.]+)(?:,\s*([\d.]+))?\)/.exec(c||''); if(!m) return null; return {r:+m[1],g:+m[2],b:+m[3],a:m[4]==null?1:+m[4]}; }
  function lum(c){ function f(v){ v/=255; return v<=0.03928? v/12.92 : Math.pow((v+0.055)/1.055,2.4); } return 0.2126*f(c.r)+0.7152*f(c.g)+0.0722*f(c.b); }
  function ratio(a,b){ var L1=lum(a),L2=lum(b),hi=Math.max(L1,L2),lo=Math.min(L1,L2); return (hi+0.05)/(lo+0.05); }
  function effBg(el){ var e=el; while(e&&e.nodeType===1){ var c=pc(getComputedStyle(e).backgroundColor); if(c&&c.a>0) return c; e=e.parentElement; } return {r:255,g:255,b:255,a:1}; }
  function accName(el){ return (el.getAttribute('aria-label')||el.getAttribute('title')||el.textContent||'').trim(); }
  function sel(el){ var s=el.tagName.toLowerCase(); if(el.id) s+='#'+el.id; else if(el.className&&typeof el.className==='string'){ var c=el.className.trim().split(/\s+/).slice(0,2).join('.'); if(c) s+='.'+c; } return s; }

  if(spec.text!=null) checks.push({name:'text',ok:scopeText.indexOf(spec.text)>=0,detail:spec.text});
  if(spec.not_text!=null) checks.push({name:'not_text',ok:scopeText.indexOf(spec.not_text)<0,detail:spec.not_text});
  if(spec.url!=null) checks.push({name:'url',ok:location.href.indexOf(spec.url)>=0,detail:location.href});
  if(spec.selector!=null){ var n=root.querySelectorAll(spec.selector).length; var min=spec.min_count||1; checks.push({name:'selector',ok:n>=min,detail:spec.selector+' -> '+n+' (min '+min+')'}); }
  if(spec.no_console_errors){ if(!A){checks.push({name:'no_console_errors',ok:false,detail:'capture not armed; call browser_capture start first'});} else { var errs=A.con.filter(function(x){return x.level==='error'||x.level==='uncaught'||x.level==='unhandledrejection';}); checks.push({name:'no_console_errors',ok:errs.length===0,detail:errs.length+' error(s)'}); } }
  if(spec.no_failed_requests){ if(!A){checks.push({name:'no_failed_requests',ok:false,detail:'capture not armed; call browser_capture start first'});} else { var bad=A.net.filter(function(x){return x.ok===false;}); checks.push({name:'no_failed_requests',ok:bad.length===0,detail:bad.length+' failed'}); } }

  if(spec.a11y){
    var ao=(typeof spec.a11y==='object')?spec.a11y:{}, ignore=ao.ignore||[], viols=[];
    function add(rule,el,detail){ if(ignore.indexOf(rule)>=0) return; var v={rule:rule,el:el?sel(el):null}; if(detail)v.detail=detail; viols.push(v); }
    if(!(document.documentElement.getAttribute('lang')||'').trim()) add('html-has-lang',document.documentElement);
    root.querySelectorAll('img').forEach(function(im){ if(im.getAttribute('aria-hidden')==='true'||im.getAttribute('role')==='presentation') return; if(im.getAttribute('alt')==null) add('image-alt',im); });
    root.querySelectorAll('input,select,textarea').forEach(function(f){ var t=(f.getAttribute('type')||'').toLowerCase(); if(t==='hidden'||t==='submit'||t==='button'||t==='reset') return; var id=f.getAttribute('id'); var lbl=(id&&document.querySelector('label[for="'+(window.CSS&&CSS.escape?CSS.escape(id):id)+'"]'))||f.closest('label')||f.getAttribute('aria-label')||f.getAttribute('aria-labelledby')||f.getAttribute('title'); if(!lbl) add('form-label',f); });
    root.querySelectorAll('button,a[href],[role="button"]').forEach(function(b){ if(!visible(b)) return; if(!accName(b)&&!b.querySelector('img[alt]:not([alt=""])')) add('control-name',b); });
    root.querySelectorAll('[tabindex]').forEach(function(t){ if(parseInt(t.getAttribute('tabindex'),10)>0) add('tabindex-positive',t); });
    var seen={}; document.querySelectorAll('[id]').forEach(function(e){ var id=e.id; if(seen[id]) add('duplicate-id',e); else seen[id]=1; });
    if(ao.target_size!==false){ root.querySelectorAll('button,a[href],[role="button"],input:not([type=hidden]),select').forEach(function(b){ if(!visible(b)) return; var r=b.getBoundingClientRect(); if(r.width<24||r.height<24) add('target-size',b,Math.round(r.width)+'x'+Math.round(r.height)); }); }
    if(ao.contrast!==false){ var textEls=[]; var walk=root.querySelectorAll('*'); for(var wi=0;wi<walk.length&&textEls.length<400;wi++){ var e=walk[wi]; if(!visible(e)) continue; var direct=''; for(var ci=0;ci<e.childNodes.length;ci++){ var cn=e.childNodes[ci]; if(cn.nodeType===3) direct+=cn.nodeValue; } if(direct.trim().length>=2) textEls.push(e); }
      var lim=ao.contrast_sample||150; for(var i=0;i<textEls.length&&i<lim;i++){ var el=textEls[i], st=getComputedStyle(el), fg=pc(st.color); if(!fg) continue; var bg=effBg(el), rr=ratio(fg,bg), fs=parseFloat(st.fontSize), bold=(parseInt(st.fontWeight,10)||400)>=700, large=(fs>=24)||(fs>=18.66&&bold), need=large?3:4.5; if(rr<need-0.05) add('contrast',el,rr.toFixed(2)+':1 (need '+need+')'); } }
    checks.push({name:'a11y',ok:viols.length===0,detail:viols.length+' violation(s)',violations:viols.slice(0,60)});
  }

  if(spec.style){
    var so=spec.style;
    function norm(c){ c=(c||'').trim(); var h=/^#([0-9a-f]{3}|[0-9a-f]{6})$/i.exec(c); if(h){ var x=h[1]; if(x.length===3) x=x[0]+x[0]+x[1]+x[1]+x[2]+x[2]; return parseInt(x.slice(0,2),16)+','+parseInt(x.slice(2,4),16)+','+parseInt(x.slice(4,6),16); } var p=pc(c); return p?(p.r+','+p.g+','+p.b):c.toLowerCase(); }
    var aCol=(so.colors||[]).map(norm), aFont=(so.fonts||[]).map(function(f){return String(f).toLowerCase();}), aSize=(so.font_sizes||[]).map(parseFloat), aSpace=(so.spacing||[]).map(parseFloat);
    var off=[], lim=so.sample_limit||600, cnt=0, els=root.querySelectorAll('*');
    for(var j=0;j<els.length&&cnt<lim;j++){ var el=els[j]; if(!visible(el)) continue; cnt++; var s=getComputedStyle(el);
      if(aCol.length){ var col=norm(s.color); if(col&&aCol.indexOf(col)<0&&off.length<80) off.push({prop:'color',value:s.color,el:sel(el)}); var bp=pc(s.backgroundColor); if(bp&&bp.a>0){ var bn=norm(s.backgroundColor); if(aCol.indexOf(bn)<0&&off.length<80) off.push({prop:'background-color',value:s.backgroundColor,el:sel(el)}); } }
      if(aFont.length){ var fam=(s.fontFamily||'').toLowerCase(); if(!aFont.some(function(a){return fam.indexOf(a)>=0;})&&off.length<80) off.push({prop:'font-family',value:s.fontFamily,el:sel(el)}); }
      if(aSize.length){ var fsz=parseFloat(s.fontSize); if(aSize.indexOf(fsz)<0&&off.length<80) off.push({prop:'font-size',value:s.fontSize,el:sel(el)}); }
      if(aSpace.length){ ['marginTop','marginRight','marginBottom','marginLeft','paddingTop','paddingRight','paddingBottom','paddingLeft'].forEach(function(p){ var v=parseFloat(s[p]); if(v>0&&aSpace.indexOf(v)<0&&off.length<80) off.push({prop:p,value:s[p],el:sel(el)}); }); }
    }
    checks.push({name:'style',ok:off.length===0,detail:off.length+' off-token value(s)',offenders:off.slice(0,60)});
  }

  if(spec.component){
    var co=spec.component, target=(spec.within?root:(co.selector?document.querySelector(co.selector):root));
    if(!target){ checks.push({name:'component',ok:false,detail:'component root not found'}); }
    else {
      if(co.visible!=null) checks.push({name:'component.visible',ok:visible(target)===!!co.visible,detail:'visible='+visible(target)});
      if(co.role!=null){ var rl=target.getAttribute('role'); checks.push({name:'component.role',ok:rl===co.role,detail:'role='+rl}); }
      if(co.states){ Object.keys(co.states).forEach(function(k){ var want=co.states[k], got;
        if(k==='disabled') got=target.disabled===true||target.getAttribute('aria-disabled')==='true';
        else if(k==='checked') got=target.checked===true||target.getAttribute('aria-checked')==='true';
        else got=(target.getAttribute('aria-'+k)==='true')||(target.getAttribute('aria-'+k)===String(want));
        checks.push({name:'component.'+k,ok:(got===want)||(String(got)===String(want)),detail:k+'='+got}); }); }
    }
  }
  return {checks:checks};
})()"##;

/// Probe that checks if DOM mutations and RAF have settled for at least 150ms.
const JS_DOM_SETTLED: &str = r##"(function(){
  if(!window.__agentctl_settle_observer){
    window.__agentctl_last_change = performance.now();
    try {
      window.__agentctl_settle_observer = new MutationObserver(function(){
        window.__agentctl_last_change = performance.now();
      });
      if(document.body){
        window.__agentctl_settle_observer.observe(document.body, {childList:true, subtree:true, attributes:true, characterData:true});
      }
    } catch(e){}
  }
  var quiet = performance.now() - (window.__agentctl_last_change || 0);
  return document.readyState === 'complete' && quiet >= 150;
})()"##;

/// In-page script that batches multiple form field updates and optional submit.
const JS_FILL_FORM: &str = r##"(async function(){
  {JS_XPATH}
  {JS_SHOWCASE_INIT}
  var fields = __FIELDS__;
  var submit = __SUBMIT__;
  var filled = 0, errors = [];
  function resolve(f){
    if(!f) return null;
    if(f.ref) return __resolve(f.ref);
    if(f.selector) return document.querySelector(f.selector);
    return null;
  }
  for(var i=0; i<fields.length; i++){
    var f = fields[i];
    var el = resolve(f);
    if(!el){
      errors.push({field: f.selector || f.ref || ('index_' + i), error: 'element not found'});
      continue;
    }
    try {
      if(el.scrollIntoView) el.scrollIntoView({block:'nearest', inline:'nearest'});
      if(el.focus) el.focus();
      var val = f.value;
      {JS_SHOWCASE_FIELD}
      var tag = (el.tagName || '').toLowerCase();
      var inputType = (el.getAttribute('type') || '').toLowerCase();
      var fType = (f.type || '').toLowerCase();
      if(tag === 'select' || fType === 'select'){
        el.value = String(val == null ? '' : val);
        el.dispatchEvent(new Event('input', {bubbles: true}));
        el.dispatchEvent(new Event('change', {bubbles: true}));
        filled++;
      } else if(inputType === 'checkbox' || inputType === 'radio' || fType === 'checkbox' || fType === 'radio'){
        var shouldCheck = Boolean(val);
        if(el.checked !== shouldCheck){
          el.checked = shouldCheck;
          el.dispatchEvent(new Event('input', {bubbles: true}));
          el.dispatchEvent(new Event('change', {bubbles: true}));
        }
        filled++;
      } else {
        if('value' in el){
          el.value = (val == null ? '' : String(val));
        } else {
          el.textContent = (val == null ? '' : String(val));
        }
        el.dispatchEvent(new Event('input', {bubbles: true}));
        el.dispatchEvent(new Event('change', {bubbles: true}));
        if(el.blur) el.blur();
        filled++;
      }
    } catch(err){
      errors.push({field: f.selector || f.ref || ('index_' + i), error: String(err)});
    }
  }
  var submitted = false;
  if(submit && errors.length === 0){
    var subEl = resolve(submit);
    if(subEl){
      {JS_SHOWCASE_SUBMIT}
      if(subEl.click) subEl.click();
      else if(subEl.form && subEl.form.requestSubmit) subEl.form.requestSubmit();
      else if(subEl.form && subEl.form.submit) subEl.form.submit();
      submitted = true;
    } else if(submit.selector || submit.ref){
      errors.push({field: 'submit', error: 'submit element not found'});
    }
  }
  return { ok: errors.length === 0, filled: filled, submitted: submitted, errors: errors };
})()"##;

/// In-page script that extracts structured data according to a schema.
const JS_EXTRACT: &str = r##"(function(){
  var schema = __SCHEMA__;
  var within = __WITHIN__;
  var root = within ? document.querySelector(within) : document;
  if(!root) return { ok: false, error: 'within root element not found: ' + within };

  function extractVal(el, rule){
    if(!el) return null;
    var attr = rule.attr || 'innerText';
    var raw;
    if(attr === 'innerText') raw = el.innerText;
    else if(attr === 'textContent') raw = el.textContent;
    else if(attr === 'value') raw = el.value;
    else raw = el.getAttribute(attr);
    if(raw == null) return null;
    raw = String(raw).trim();
    if(rule.regex){
      var m = new RegExp(rule.regex).exec(raw);
      if(!m) return null;
      return m[1] != null ? m[1] : m[0];
    }
    return raw;
  }

  function extractObject(node, rules){
    var res = {};
    for(var k in rules){
      var r = rules[k];
      if(typeof r === 'string'){
        r = { selector: r };
      }
      if(r.multiple){
        var items = [];
        var matches = node.querySelectorAll(r.selector || '*');
        for(var j=0; j<matches.length; j++){
          if(r.fields){
            items.push(extractObject(matches[j], r.fields));
          } else {
            items.push(extractVal(matches[j], r));
          }
        }
        res[k] = items;
      } else {
        var targetEl = r.selector ? node.querySelector(r.selector) : node;
        if(r.fields){
          res[k] = targetEl ? extractObject(targetEl, r.fields) : null;
        } else {
          res[k] = extractVal(targetEl, r);
        }
      }
    }
    return res;
  }

  try {
    var data = extractObject(root, schema);
    return { ok: true, data: data };
  } catch(e) {
    return { ok: false, error: String(e) };
  }
})()"##;

#[async_trait]
impl BrowserBackend for CdpBackend {
    async fn connect(
        &self,
        attach_port: Option<u16>,
        launch: Option<Value>,
    ) -> Result<Value, BrowserError> {
        // Multi-engine check: Safari (WebKit) via Apple's safaridriver W3C WebDriver
        if let Some(ref spec) = launch {
            let browser_name = spec
                .get("browser")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_lowercase();
            // Only Chromium-family (auto-discovered) and Safari can be
            // launched. Any other name used to fall through to "whatever
            // Chromium is installed", which silently ignored the request.
            if !matches!(browser_name.as_str(), "" | "chromium" | "safari" | "webkit") {
                return Err(BrowserError::Unsupported(format!(
                    "launch.browser '{browser_name}' is not supported; use 'chromium' (default) or 'safari'"
                )));
            }
            if browser_name != "safari" && browser_name != "webkit" && spec.get("url").is_some() {
                return Err(BrowserError::Unsupported(
                    "launch.url is only supported with launch.browser='safari'; for Chromium connect, then use browser_navigate"
                        .into(),
                ));
            }
            if browser_name == "safari" || browser_name == "webkit" {
                if !crate::safari::is_safari_available() {
                    return Err(BrowserError::Unsupported(
                        "Safari WebDriver is only supported on macOS with safaridriver installed"
                            .into(),
                    ));
                }
                let port = match spec.get("port").and_then(Value::as_u64) {
                    None => None,
                    Some(p) => match u16::try_from(p) {
                        Ok(p) if p > 0 => Some(p),
                        _ => {
                            return Err(BrowserError::Failed(format!(
                                "launch.port {p} is not a valid TCP port (1-65535)"
                            )))
                        }
                    },
                };
                // Judge the first URL before starting a driver, so a denied
                // URL costs nothing and leaves nothing running.
                let initial_url = spec.get("url").and_then(Value::as_str);
                if let Some(u) = initial_url {
                    if let Err(denied) = self.nav.check(u).await {
                        return Err(BrowserError::PermissionDenied(denied.message()));
                    }
                }
                let diagnose = spec
                    .get("diagnose")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let proc =
                    crate::safari::SafariProcess::launch(&crate::safari::SafariDriverConfig {
                        port,
                        diagnose,
                    })
                    .await?;
                let driver_port = proc.port;
                let session = crate::safari::SafariSession::create(driver_port).await?;
                if let Some(u) = initial_url {
                    if let Err(e) = session.navigate(u).await {
                        let _ = session.close().await;
                        return Err(e);
                    }
                }

                let id = self.next_id.fetch_add(1, Ordering::SeqCst);
                let entry = std::sync::Arc::new(SafariEntry {
                    proc: Mutex::new(Some(proc)),
                    session,
                });
                self.safari_sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id, entry);

                return Ok(json!({
                    "browser_id": id,
                    "host": "127.0.0.1",
                    "port": driver_port,
                    "browser": "Safari",
                    "engine": "webkit",
                    "driver": "safaridriver",
                    "target_id": format!("safari-{id}"),
                }));
            }
        }

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
                    host: host.clone(),
                    port,
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
        // Close the sessions that watch tabs; their new-document scripts go
        // with them.
        if let Ok(mut m) = self.observers.lock() {
            for (_, o) in m.drain() {
                o.task.abort();
            }
        }
        self.reap_all();
    }

    async fn disconnect(&self, browser_id: u32, kill: bool) -> Result<Value, BrowserError> {
        // An attached browser outlives us, so the demo overlay must not stay
        // behind in its pages. (A browser we kill takes the overlay with it.)
        let showcase_on = self
            .showcase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .enabled;
        if showcase_on && !kill {
            self.teardown_showcase(Some(browser_id)).await;
        }
        // Check if browser_id belongs to a Safari session
        let safari_entry = {
            let mut g = self
                .safari_sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            g.remove(&browser_id)
        };
        if let Some(entry) = safari_entry {
            let _ = entry.session.close().await;
            if kill {
                if let Ok(mut proc_opt) = entry.proc.lock() {
                    if let Some(mut p) = proc_opt.take() {
                        p.kill();
                    }
                }
            }
            return Ok(json!({
                "disconnected": browser_id,
                "engine": "webkit",
                "killed": kill,
            }));
        }

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
                // Graceful CDP quit first, so a sandboxed browser's whole
                // process tree goes down, not just the launcher we hold.
                let _ = browser_close(&l.host, l.port).await;
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
        if let Ok(safari_entry) = self.get_safari_session(&format!("safari-{browser_id}")) {
            match action {
                "list" => {
                    let u = safari_entry
                        .session
                        .get_url()
                        .await
                        .unwrap_or_else(|_| "about:blank".into());
                    let title = safari_entry.session.get_title().await.unwrap_or_default();
                    return Ok(json!({
                        "tabs": [
                            {
                                "target_id": format!("safari-{browser_id}"),
                                "title": title,
                                "url": u,
                            }
                        ]
                    }));
                }
                "open" => {
                    if let Some(u) = url {
                        if let Err(denied) = self.nav.check(u).await {
                            return Err(BrowserError::PermissionDenied(denied.message()));
                        }
                        safari_entry.session.navigate(u).await?;
                    }
                    return Ok(json!({
                        "target_id": format!("safari-{browser_id}"),
                        "url": url.unwrap_or("about:blank")
                    }));
                }
                "activate" => {
                    return Ok(json!({ "activated": format!("safari-{browser_id}") }));
                }
                "close" => {
                    safari_entry.session.close().await?;
                    return Ok(json!({ "closed": format!("safari-{browser_id}") }));
                }
                other => {
                    return Err(BrowserError::Failed(format!(
                        "unknown tabs action '{other}'"
                    )))
                }
            }
        }

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
                // Chrome loads the URL as it creates the tab, so the policy
                // must run first, as it does for `navigate goto`.
                if let Err(denied) = self.nav.check(u).await {
                    return Err(BrowserError::PermissionDenied(denied.message()));
                }
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
        if target.starts_with("safari-") {
            let entry = self.get_safari_session(target)?;
            match action {
                "goto" => {
                    let u = url.ok_or_else(|| BrowserError::Failed("goto needs 'url'".into()))?;
                    if let Err(denied) = self.nav.check(u).await {
                        return Err(BrowserError::PermissionDenied(denied.message()));
                    }
                    entry.session.navigate(u).await?;
                    return Ok(json!({ "url": u, "engine": "webkit" }));
                }
                "reload" => {
                    entry.session.refresh().await?;
                    return Ok(json!({ "reloaded": true, "engine": "webkit" }));
                }
                "back" => {
                    entry.session.back().await?;
                    return Ok(json!({ "went_back": true, "engine": "webkit" }));
                }
                "forward" => {
                    entry.session.forward().await?;
                    return Ok(json!({ "went_forward": true, "engine": "webkit" }));
                }
                other => {
                    return Err(BrowserError::Failed(format!(
                        "unknown navigate action '{other}'"
                    )))
                }
            }
        }

        let mut c = self.conn(target).await?;
        let mut out = match action {
            "goto" => {
                let u = url.ok_or_else(|| BrowserError::Failed("goto needs 'url'".into()))?;
                if let Err(denied) = self.nav.check(u).await {
                    return Err(BrowserError::PermissionDenied(denied.message()));
                }
                // Mark the document being left, so a `wait navigation` that
                // follows waits for the new one rather than trusting the old
                // document's `readyState`.
                self.plant_nav_token(&mut c, target, true).await;
                let r = c.call("Page.navigate", json!({ "url": u })).await;
                let r = match r {
                    Ok(r) => r,
                    Err(e) => {
                        self.clear_nav_pending(target, None);
                        return Err(e);
                    }
                };
                if let Some(err) = r.get("errorText").and_then(Value::as_str) {
                    self.clear_nav_pending(target, None);
                    return Err(BrowserError::Failed(format!("navigate: {err}")));
                }
                // A fragment-only change keeps the document (no loaderId), so
                // there is no new document to wait for.
                if r.get("loaderId").is_none() {
                    self.clear_nav_pending(target, None);
                }
                json!({ "url": u, "frameId": r.get("frameId") })
            }
            "reload" => {
                self.plant_nav_token(&mut c, target, true).await;
                if let Err(e) = c.call("Page.reload", json!({})).await {
                    self.clear_nav_pending(target, None);
                    return Err(e);
                }
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
        let is_safari = target.starts_with("safari-");
        if is_safari && mode == "text" {
            let entry = self.get_safari_session(target)?;
            let v = entry.session.execute_sync(
                "return ({url:location.href,title:document.title,text:(document.body?document.body.innerText:'').slice(0,20000)});",
                &[],
            ).await?;
            return Ok(v);
        }

        let c_opt = if is_safari {
            None
        } else {
            let mut c = self.conn(target).await?;
            if mode == "text" {
                let v = Self::eval_value(
                    &mut c,
                    "({url:location.href,title:document.title,text:(document.body?document.body.innerText:'').slice(0,20000)})",
                )
                .await?;
                return Ok(v);
            }
            Some(c)
        };
        // dom / accessibility both use a DOM flatten of interactable/labeled nodes.
        let root_arg = serde_json::to_string(&root).unwrap_or_else(|_| "null".into());
        let expr = format!(
            r#"(function(){{
  {JS_XPATH}
  var rootSel={root_arg};
  var base=(rootSel && document.querySelector(rootSel)) || document.body;
  if(!base) return {{url:location.href,title:document.title,nodes:[]}};

  function deriveIntent(el, role, tag, name){{
    if(!el) return null;
    var di = el.getAttribute ? (el.getAttribute('data-intent') || el.getAttribute('data-action') || el.getAttribute('data-testid')) : null;
    if(di) return String(di);
    var aria = el.getAttribute ? el.getAttribute('aria-label') : null;
    if(aria && (role === 'button' || tag === 'button')) {{
      return aria.toLowerCase().replace(/[^a-z0-9]+/g, '_').replace(/^_+|_+$/g, '');
    }}
    var id = el.id;
    if(id && (id.indexOf('btn') >= 0 || id.indexOf('submit') >= 0 || id.indexOf('checkout') >= 0 || id.indexOf('search') >= 0 || id.indexOf('cart') >= 0 || id.indexOf('login') >= 0)) {{
      return id.toLowerCase().replace(/[^a-z0-9]+/g, '_');
    }}
    if(name && (role === 'button' || tag === 'button')) {{
      var n = name.toLowerCase().trim();
      if(n.indexOf('checkout') >= 0) return 'checkout_order';
      if(n.indexOf('submit') >= 0) return 'submit_form';
      if(n.indexOf('buy') >= 0 || n.indexOf('order') >= 0) return 'place_order';
      if(n.indexOf('login') >= 0 || n.indexOf('sign in') >= 0) return 'login_auth';
      if(n.indexOf('search') >= 0) return 'search_query';
      if(n.indexOf('add to cart') >= 0) return 'add_to_cart';
      return n.replace(/[^a-z0-9]+/g, '_').slice(0, 32);
    }}
    return null;
  }}

  function extractBoundState(el){{
    if(!el) return null;
    var ds = el.getAttribute ? (el.getAttribute('data-state') || el.getAttribute('data-bound')) : null;
    if(ds){{
      try {{ return JSON.parse(ds); }} catch(e){{ return {{ state: ds }}; }}
    }}
    if(el.__agentctl_bound_state) return el.__agentctl_bound_state;
    for(var k in el){{
      if(k.startsWith('__reactFiber$') || k.startsWith('__reactInternalInstance$')){{
        var fiber = el[k];
        if(fiber && fiber.memoizedProps){{
          var p = fiber.memoizedProps;
          if(p.state || p.data || p.cart || p.model) return p.state || p.data || p.cart || p.model;
        }}
      }}
      if(k.startsWith('__reactProps$')){{
        var props = el[k];
        if(props && (props.state || props.data || props.cart || props.model)) return props.state || props.data || props.cart || props.model;
      }}
    }}
    var intent = (el.getAttribute ? (el.getAttribute('data-intent') || el.id || el.innerText || '') : '').toLowerCase();
    if(intent.indexOf('checkout') >= 0 || intent.indexOf('cart') >= 0){{
      if(window.useCartStore && typeof window.useCartStore.getState === 'function'){{
        try {{ return window.useCartStore.getState(); }} catch(e){{}}
      }}
      if(window.useStore && typeof window.useStore.getState === 'function'){{
        try {{ return window.useStore.getState(); }} catch(e){{}}
      }}
      if(window.__agentctl_state_hook && typeof window.__agentctl_state_hook.getState === 'function'){{
        try {{ return window.__agentctl_state_hook.getState(); }} catch(e){{}}
      }}
      if(window.store && typeof window.store.getState === 'function'){{
        try {{ return window.store.getState(); }} catch(e){{}}
      }}
    }}
    return null;
  }}

  var INTERACT={{A:1,BUTTON:1,INPUT:1,SELECT:1,TEXTAREA:1,SUMMARY:1,LABEL:1,OPTION:1,CANVAS:1}};
  var all = [];
  function collect(root){{
    if(!root) return;
    var nodes = root.querySelectorAll ? root.querySelectorAll('*') : [];
    for(var k=0; k<nodes.length; k++){{
      all.push(nodes[k]);
      if(nodes[k].shadowRoot){{
        collect(nodes[k].shadowRoot);
      }}
    }}
  }}
  collect(base);

  var out = [];
  for(var i=0; i<all.length && out.length<400; i++){{
    var el = all[i], tag = el.tagName, role = el.getAttribute ? el.getAttribute('role') : null;
    var isCanvas = (tag === 'CANVAS');
    var interactive = INTERACT[tag] || role || (el.getAttribute && el.getAttribute('tabindex') !== null) || el.isContentEditable || typeof el.onclick === 'function';
    if(!interactive && !isCanvas) continue;
    var rect = el.getBoundingClientRect();
    if(rect.width === 0 && rect.height === 0) continue;

    // Canvases that publish their interactive regions get child nodes; any
    // other canvas stays an opaque node (no pixel analysis happens here).
    if(isCanvas){{
      var regions = __canvas_regions(el);
      if(regions && regions.length > 0){{
        var canvasXp = __xp(el);
        for(var r=0; r<regions.length && out.length<400; r++){{
          var reg = regions[r];
          var regId = __canvas_reg_id(reg, r);
          var regName = String(reg.label || reg.text || reg.name || reg.id || 'Canvas Button');
          var regRole = reg.role || 'button';
          var box = __canvas_box(el, reg);
          var regIntent = reg.intent || reg.semantic_intent || ('canvas_' + regName.toLowerCase().replace(/[^a-z0-9]+/g, '_'));
          out.push({{
            ref: canvasXp + '::canvas[' + __canvas_enc(regId) + ']',
            tag: 'canvas-child',
            role: regRole,
            name: regName,
            x: Math.round(box.x),
            y: Math.round(box.y),
            w: Math.round(box.w),
            h: Math.round(box.h),
            semantic_intent: regIntent,
            bound_state: reg.bound_state || reg.state || null,
            is_enabled: reg.disabled !== true
          }});
        }}
        continue;
      }}
    }}

    var name = (el.getAttribute ? (el.getAttribute('aria-label') || el.getAttribute('placeholder') || el.value || el.innerText || el.getAttribute('title') || '') : '').trim().slice(0, 120);
    var isEnabled = el.disabled !== true && (!el.getAttribute || el.getAttribute('aria-disabled') !== 'true');
    out.push({{
      ref: __xp(el),
      tag: tag.toLowerCase(),
      role: role || null,
      name: name,
      x: Math.round(rect.x),
      y: Math.round(rect.y),
      w: Math.round(rect.width),
      h: Math.round(rect.height),
      semantic_intent: deriveIntent(el, role, tag.toLowerCase(), name),
      bound_state: extractBoundState(el),
      is_enabled: isEnabled
    }});
  }}
  return {{url:location.href,title:document.title,mode:{mode:?},nodes:out}};
}})()"#
        );
        if is_safari {
            let entry = self.get_safari_session(target)?;
            let v = entry
                .session
                .execute_sync(&format!("return {expr};"), &[])
                .await?;
            return Ok(v);
        }
        let mut c = c_opt.unwrap();
        Self::eval_value(&mut c, &expr).await
    }

    async fn query(
        &self,
        target: &str,
        by: &str,
        query: &str,
        all: bool,
    ) -> Result<Value, BrowserError> {
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
        let v = if target.starts_with("safari-") {
            let entry = self.get_safari_session(target)?;
            entry
                .session
                .execute_sync(&format!("return {expr};"), &[])
                .await?
        } else {
            let mut c = self.conn(target).await?;
            Self::eval_value(&mut c, &expr).await?
        };
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
        self.act_masked(target, locator, action, value, false).await
    }

    async fn act_masked(
        &self,
        target: &str,
        locator: Locator<'_>,
        action: &str,
        value: Option<&str>,
        secret: bool,
    ) -> Result<Value, BrowserError> {
        let is_safari = target.starts_with("safari-");
        let press_key = if action == "press" {
            if is_safari {
                return Err(BrowserError::Unsupported(
                    "act 'press' is not supported on the Safari engine".into(),
                ));
            }
            Some(key_event_spec(value.unwrap_or("")).ok_or_else(|| {
                BrowserError::Failed(format!(
                    "act 'press' needs a supported key in 'value' (Enter, Escape, Tab), got '{}'",
                    value.unwrap_or("")
                ))
            })?)
        } else {
            None
        };
        let mut c_opt = if is_safari {
            None
        } else {
            Some(self.conn(target).await?)
        };
        // Resolve to an element in the same eval: a `ref` via XPath, or a
        // selector via `__find`, so a scripted action is one round trip.
        let resolve = match locator {
            Locator::Ref(r) => {
                format!(
                    "__resolve({})",
                    serde_json::to_string(r).unwrap_or_else(|_| "\"\"".into())
                )
            }
            Locator::Selector {
                by,
                query,
                within,
                text,
                index,
            } => {
                let by_json = serde_json::to_string(by).unwrap_or_else(|_| "\"css\"".into());
                let query_json = serde_json::to_string(query).unwrap_or_else(|_| "\"\"".into());
                let within_json = serde_json::to_string(&within).unwrap_or_else(|_| "null".into());
                let text_json = serde_json::to_string(&text).unwrap_or_else(|_| "null".into());
                let index_json = serde_json::to_string(&index).unwrap_or_else(|_| "null".into());
                format!("__find({by_json},{query_json},{within_json},{text_json},{index_json})")
            }
        };
        let act = serde_json::to_string(action).unwrap_or_else(|_| "\"click\"".into());
        let val = serde_json::to_string(&value).unwrap_or_else(|_| "null".into());

        let showcase_cfg = self
            .showcase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let (showcase_init, showcase_call) = if showcase_cfg.enabled {
            let glide_ms = showcase_cfg.glide_ms();
            let click_ripple = showcase_cfg.click_ripple;
            let typing_hud = showcase_cfg.typing_hud;
            (
                crate::showcase::JS_SHOWCASE_ENGINE,
                format!(
                    r#"
  try {{
    if(typeof window !== 'undefined' && window.__agentctl_showcase && typeof window.__agentctl_showcase.act === 'function') {{
      var b = el.getBoundingClientRect();
      var cx = Math.round(b.left + b.width / 2);
      var cy = Math.round(b.top + b.height / 2);
      await window.__agentctl_showcase.act(cx, cy, action, value, {glide_ms}, {click_ripple}, {typing_hud}, el, {secret});
    }}
  }} catch(e) {{}}
"#
                ),
            )
        } else {
            ("", String::new())
        };

        // A click, submit or key press may start a navigation. Mark the
        // document it happens on, so a following `wait navigation` can tell
        // that document from the one it lands on.
        let nav_token =
            (!is_safari && matches!(action, "click" | "submit" | "press")).then(new_nav_token);
        let nav_mark = nav_token
            .as_ref()
            .map(|t| format!("window.__agentctl_nav_token = {t:?};"))
            .unwrap_or_default();

        let expr = format!(
            r#"(async function(){{
  {JS_XPATH}
  {JS_FIND}
  {showcase_init}
  var el, action={act}, value={val};
  try {{ el = {resolve}; }} catch(e) {{ return {{ok:false,error:String(e && e.message ? e.message : e)}}; }}
  if(!el) return {{ok:false,error:'element not found'}};
  if(el.__is_canvas_target){{
    var c = el.canvas;
    if(action !== 'click' && action !== 'hover' && action !== 'scroll_into_view'){{
      return {{ok:false,kind:'unsupported',error:"action '"+action+"' is not supported on a canvas region (only click, hover, scroll_into_view); the region is drawn pixels, not a DOM element"}};
    }}
    try{{ c.scrollIntoView({{block:'center',inline:'center',behavior:'instant'}}); }}catch(e){{}}
    var box = __canvas_box(c, el.reg);
    var px = box.x + box.w / 2, py = box.y + box.h / 2;
    if(action === 'scroll_into_view') return {{ok:true,action:action,canvas_target:true}};
    var vw = document.documentElement.clientWidth, vh = document.documentElement.clientHeight;
    if(!(px >= 0 && py >= 0 && px < vw && py < vh)){{
      return {{ok:false,kind:'failed',error:'canvas region centre ('+px.toFixed(1)+','+py.toFixed(1)+') is outside the '+vw+'x'+vh+' viewport; nothing was clicked'}};
    }}
    var rootNode = c.getRootNode ? c.getRootNode() : document;
    var hit = (rootNode.elementFromPoint ? rootNode : document).elementFromPoint(px, py);
    if(hit !== c){{
      return {{ok:false,kind:'failed',error:'canvas region centre ('+px.toFixed(1)+','+py.toFixed(1)+') is covered by another element or the canvas does not receive pointer events; nothing was clicked'}};
    }}
    return {{ok:true,action:action,canvas_target:true,canvas_point:true,x:px,y:py}};
  }}
  try{{ el.scrollIntoView({{block:'center',inline:'center'}}); }}catch(e){{}}
  {showcase_call}
  {nav_mark}
  switch(action){{
    case 'click': el.click(); break;
    case 'focus': el.focus(); break;
    case 'press': el.focus(); break;
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
  return {{ok:true,action:action,showcase:{}}};
}})()"#,
            showcase_cfg.enabled
        );
        let mut v = if is_safari {
            let entry = self.get_safari_session(target)?;
            // The script is an async IIFE: `execute/sync` would not await it.
            entry.session.eval_promise(&expr).await?
        } else {
            let c = c_opt.as_mut().unwrap();
            Self::eval_value(c, &expr).await?
        };
        // Success must be affirmed. A missing `ok` (for instance `{}` from an
        // un-awaited promise) is a failure, never a silent success.
        if v.get("ok").and_then(Value::as_bool) != Some(true) {
            let msg = v
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("action returned no result")
                .to_string();
            return Err(match v.get("kind").and_then(Value::as_str) {
                Some("unsupported") => BrowserError::Unsupported(msg),
                Some("failed") => BrowserError::Failed(msg),
                _ => BrowserError::NotFound(msg),
            });
        }
        if let Some(token) = nav_token {
            // The marker is on the document the action ran on; a click only
            // *might* navigate, so the wait for it is bounded (NAV_EXPECT_MS).
            self.set_nav_pending(target, token, false);
        }
        if v.get("canvas_point").and_then(Value::as_bool) == Some(true) {
            // A canvas region is only pixels: the click must be a real,
            // trusted pointer input at the computed point.
            let Some(c) = c_opt.as_mut() else {
                return Err(BrowserError::Unsupported(
                    "canvas region input needs the CDP (Chrome) engine; the WebKit engine cannot send trusted pointer input here".into(),
                ));
            };
            let (x, y) = (
                v.get("x").and_then(Value::as_f64).unwrap_or(0.0),
                v.get("y").and_then(Value::as_f64).unwrap_or(0.0),
            );
            Self::cdp_mouse(c, x, y, action == "click").await?;
            if let Some(map) = v.as_object_mut() {
                map.insert("input".into(), json!("cdp"));
            }
        }
        if let (Some(spec), Some(c)) = (press_key, c_opt.as_mut()) {
            // The element is focused by the eval above; send a real key
            // press so default actions (implicit form submit, focus move) run.
            let mut down = json!({
                "type": if spec.text.is_some() { "keyDown" } else { "rawKeyDown" },
                "key": spec.key,
                "code": spec.code,
                "windowsVirtualKeyCode": spec.vk,
                "nativeVirtualKeyCode": spec.vk,
            });
            if let (Some(t), Some(m)) = (spec.text, down.as_object_mut()) {
                m.insert("text".into(), json!(t));
            }
            c.call("Input.dispatchKeyEvent", down).await?;
            c.call(
                "Input.dispatchKeyEvent",
                json!({
                    "type": "keyUp",
                    "key": spec.key,
                    "code": spec.code,
                    "windowsVirtualKeyCode": spec.vk,
                    "nativeVirtualKeyCode": spec.vk,
                }),
            )
            .await?;
            if let Some(m) = v.as_object_mut() {
                m.insert("key".into(), json!(spec.key));
            }
        }
        if let Some(ref mut c) = c_opt {
            self.note_dialogs(target, c, &mut v);
        }
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
        if target.starts_with("safari-") {
            let entry = self.get_safari_session(target)?;
            let deadline = Instant::now() + Duration::from_millis(timeout_ms.clamp(50, 60_000));
            let probe = match cond {
                "selector" => {
                    let s = arg.ok_or_else(|| {
                        BrowserError::Failed("wait selector needs a value".into())
                    })?;
                    let sl = serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
                    format!("return !!document.querySelector({sl});")
                }
                "navigation" | "network_idle" => {
                    "return document.readyState==='complete';".to_string()
                }
                "dom_settled" => format!("return {JS_DOM_SETTLED};"),
                "htmx_settled" => format!("return {JS_HTMX_SETTLED};"),
                other => {
                    return Err(BrowserError::Failed(format!(
                        "unknown wait condition '{other}'"
                    )))
                }
            };
            loop {
                let hit = entry.session.execute_sync(&probe, &[]).await?;
                if hit.as_bool() == Some(true) {
                    if cond == "network_idle" {
                        sleep(Duration::from_millis(400)).await;
                    }
                    return Ok(json!({ "settled": true, "condition": cond, "engine": "webkit" }));
                }
                if Instant::now() >= deadline {
                    return Err(BrowserError::Timeout(format!(
                        "wait '{cond}' did not settle in {timeout_ms}ms"
                    )));
                }
                sleep(Duration::from_millis(150)).await;
            }
        }

        let mut c = self.conn(target).await?;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.clamp(50, 60_000));
        // A goto, reload or click has marked the document it left: wait for
        // the new one. Without a marker there is nothing to tell the old
        // document from the new, so the plain `readyState` check below stands.
        if cond == "navigation" {
            if let Some(pending) = self.nav_pending_for(target) {
                return self
                    .wait_replaced_document(target, &mut c, pending, timeout_ms)
                    .await;
            }
        }
        if cond == "challenge_cleared" || cond == "challenge" {
            let res =
                crate::challenge::ChallengeManager::wait_for_clearance(self, target, timeout_ms)
                    .await?;
            let mut out = json!({ "settled": true, "condition": cond, "challenge": res });
            self.note_dialogs(target, &mut c, &mut out);
            return Ok(out);
        }

        let probe = match cond {
            "selector" => {
                let s =
                    arg.ok_or_else(|| BrowserError::Failed("wait selector needs a value".into()))?;
                let sl = serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
                format!("!!document.querySelector({sl})")
            }
            "navigation" => "document.readyState==='complete'".to_string(),
            "network_idle" => "document.readyState==='complete'".to_string(),
            "dom_settled" => JS_DOM_SETTLED.to_string(),
            "htmx_settled" => JS_HTMX_SETTLED.to_string(),
            other => {
                return Err(BrowserError::Failed(format!(
                    "unknown wait condition '{other}'"
                )))
            }
        };
        loop {
            let hit = Self::eval_value(&mut c, &probe).await.map_err(|e| {
                // The probe throws this when `window.htmx` is missing.
                if cond == "htmx_settled" && err_msg(&e).contains("htmx not present") {
                    BrowserError::NotFound("htmx not present on page".into())
                } else {
                    e
                }
            })?;
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
        if target.starts_with("safari-") {
            let entry = self.get_safari_session(target)?;
            let Some(r) = node_ref else {
                // Whole page: size is "not measured", as documented on `Shot`.
                let b64 = entry.session.screenshot().await?;
                return Ok(Shot {
                    base64: b64,
                    width: 0,
                    height: 0,
                });
            };
            let xp = serde_json::to_string(r).unwrap_or_else(|_| "\"\"".into());
            let expr = format!(
                r#"(function(){{
  {JS_XPATH}
  var el=__resolve({xp}); if(!el) return null;
  el.scrollIntoView({{block:'center'}});
  var b=el.getBoundingClientRect();
  return [el,{{w:b.width,h:b.height}}];
}})()"#
            );
            let found = entry
                .session
                .execute_sync(&format!("return {expr};"), &[])
                .await?;
            let (Some(el), Some(dims)) = (found.get(0), found.get(1)) else {
                return Err(BrowserError::NotFound(format!("ref '{r}' not found")));
            };
            let b64 = entry.session.element_screenshot(el).await?;
            return Ok(Shot {
                base64: b64,
                width: dims.get("w").and_then(Value::as_f64).unwrap_or(0.0) as u32,
                height: dims.get("h").and_then(Value::as_f64).unwrap_or(0.0) as u32,
            });
        }

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

    async fn set_viewport(
        &self,
        target: &str,
        width: u32,
        height: u32,
        mobile: bool,
        scale: f64,
    ) -> Result<Value, BrowserError> {
        if target.starts_with("safari-") {
            let entry = self.get_safari_session(target)?;
            // WebDriver can only resize the window. It cannot clear an override,
            // emulate a mobile device, or change the device scale factor, so
            // those are refused rather than reported as applied.
            if mobile {
                return Err(BrowserError::Unsupported(
                    "mobile emulation is not available through Safari WebDriver".into(),
                ));
            }
            if scale > 0.0 && (scale - 1.0).abs() > f64::EPSILON {
                return Err(BrowserError::Unsupported(
                    "device scale factor emulation is not available through Safari WebDriver"
                        .into(),
                ));
            }
            if width == 0 && height == 0 {
                return Err(BrowserError::Unsupported(
                    "Safari WebDriver cannot clear a viewport override (it only resizes the window)"
                        .into(),
                ));
            }
            if width == 0 || height == 0 {
                return Err(BrowserError::Failed(
                    "set_viewport needs both width and height greater than zero".into(),
                ));
            }
            entry.session.set_window_rect(width, height).await?;
            return Ok(json!({
                "width": width,
                "height": height,
                "mobile": false,
                "device_scale_factor": 1.0,
                "engine": "webkit",
                "note": "window resized; the page viewport may differ by the browser chrome"
            }));
        }

        let mut c = self.conn(target).await?;
        if width == 0 {
            c.call("Emulation.clearDeviceMetricsOverride", json!({}))
                .await?;
            return Ok(json!({ "cleared": true }));
        }
        let dsf = if scale > 0.0 { scale } else { 1.0 };
        c.call(
            "Emulation.setDeviceMetricsOverride",
            json!({
                "width": width,
                "height": height,
                "deviceScaleFactor": dsf,
                "mobile": mobile,
            }),
        )
        .await?;
        Ok(json!({
            "width": width,
            "height": height,
            "mobile": mobile,
            "device_scale_factor": dsf,
        }))
    }

    async fn eval(&self, target: &str, expression: &str) -> Result<Value, BrowserError> {
        if target.starts_with("safari-") {
            let entry = self.get_safari_session(target)?;
            let script = if expression.trim().starts_with("return ")
                || expression.trim().starts_with("function")
            {
                expression.to_string()
            } else {
                format!("return ({expression});")
            };
            let v = entry.session.execute_sync(&script, &[]).await?;
            return Ok(json!({ "result": v, "engine": "webkit" }));
        }

        let mut c = self.conn(target).await?;
        let v = Self::eval_value(&mut c, expression).await?;
        let mut out = json!({ "result": v });
        self.note_dialogs(target, &mut c, &mut out);
        Ok(out)
    }

    async fn observe_start(
        &self,
        target: &str,
        binding: &str,
        new_document_script: &str,
        current_document_script: &str,
        dialogs: Option<RecordDialogs>,
    ) -> Result<Value, BrowserError> {
        if target.starts_with("safari-") {
            return Err(BrowserError::Unsupported(
                "recording needs the CDP (Chrome) engine; the WebKit engine has no persistent session".into(),
            ));
        }
        {
            let mut m = self.observers.lock().map_err(|_| poisoned())?;
            if let Some(o) = m.get(target) {
                if !o.task.is_finished() {
                    return Ok(json!({
                        "installed": true, "already_active": true, "url": o.start_url
                    }));
                }
                m.remove(target);
            }
        }
        // Who answers dialogs. A person can only at a visible window, so the
        // default is `Human` there and the tab's policy in a headless
        // browser; asking for `Human` in a headless one would hang the tab.
        let headless = self.target_is_headless(target).await;
        let tab_policy = self.dialog_policy(target);
        let mode = match dialogs {
            Some(RecordDialogs::Human) if headless => {
                return Err(BrowserError::Unsupported(
                    "dialogs 'human' needs a visible browser window: this browser is headless, so nobody could answer a dialog and the tab would hang. Use 'accept' or 'dismiss'".into(),
                ))
            }
            Some(m) => m,
            None if !headless => RecordDialogs::Human,
            None if matches!(tab_policy, DialogPolicy::Accept(_)) => RecordDialogs::Accept,
            None => RecordDialogs::Dismiss,
        };
        let ws = self.resolve_ws(target).await?;
        let mut c = CdpConn::connect(&ws).await?;
        // `Page.enable` is not optional here: Chrome only applies a
        // new-document script while a Page-domain client is attached (it is
        // dropped by `Page.disable`). The cost is that this session becomes
        // one Chrome announces the page's JavaScript dialogs to. Chrome also
        // shows the dialog natively, so with `Human` the recorder just
        // listens; otherwise it answers (the tab's typed text for an accept).
        // `observe_stop` lists the dialogs and how they ended.
        match mode {
            RecordDialogs::Human => c.leave_dialogs_to_person(true),
            RecordDialogs::Dismiss => c.set_dialog_policy(DialogPolicy::Dismiss),
            RecordDialogs::Accept => c.set_dialog_policy(match tab_policy {
                DialogPolicy::Accept(t) => DialogPolicy::Accept(t),
                DialogPolicy::Dismiss => DialogPolicy::Accept(None),
            }),
        }
        c.keep_events(true);
        c.call("Runtime.enable", json!({})).await?;
        // The binding exists only in the recorder's isolated world, never in
        // the page's main world, so a page cannot forge recorded steps.
        c.call(
            "Runtime.addBinding",
            json!({ "name": binding, "executionContextName": RECORDER_WORLD }),
        )
        .await?;
        c.call("Page.enable", json!({})).await?;
        let added = c
            .call(
                "Page.addScriptToEvaluateOnNewDocument",
                json!({ "source": new_document_script, "worldName": RECORDER_WORLD }),
            )
            .await?;
        let script_id = added
            .get("identifier")
            .and_then(Value::as_str)
            .map(String::from);
        // The page that is already loaded gets the recorder in the same world:
        // create (or reuse) it in the top frame and run the script there.
        let tree = c.call("Page.getFrameTree", json!({})).await?;
        let frame_id = tree
            .get("frameTree")
            .and_then(|t| t.get("frame"))
            .and_then(|f| f.get("id"))
            .and_then(Value::as_str)
            .ok_or_else(|| BrowserError::Failed("recorder install: no top frame".into()))?
            .to_string();
        let world = c
            .call(
                "Page.createIsolatedWorld",
                json!({ "frameId": frame_id, "worldName": RECORDER_WORLD }),
            )
            .await?;
        let context_id = world
            .get("executionContextId")
            .and_then(Value::as_i64)
            .ok_or_else(|| BrowserError::Failed("recorder install: no isolated world".into()))?;
        let r = c
            .call(
                "Runtime.evaluate",
                json!({
                    "expression": current_document_script,
                    "contextId": context_id,
                    "returnByValue": true,
                    "awaitPromise": true
                }),
            )
            .await?;
        let mut worlds = std::collections::HashSet::new();
        worlds.insert(context_id);
        let early = c.take_events();
        c.keep_events(false);
        if let Some(exc) = r.get("exceptionDetails") {
            let text = exc
                .get("exception")
                .and_then(|e| e.get("description"))
                .and_then(Value::as_str)
                .or_else(|| exc.get("text").and_then(Value::as_str))
                .unwrap_or("javascript error");
            return Err(BrowserError::Failed(format!("recorder install: {text}")));
        }
        let installed = r
            .get("result")
            .and_then(|o| o.get("value"))
            .cloned()
            .unwrap_or(Value::Null);
        let start_url = installed
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        let events = std::sync::Arc::new(Mutex::new(Vec::new()));
        let started = std::time::Instant::now();
        let (stop, stop_rx) = tokio::sync::oneshot::channel();
        let watch = Watch {
            binding: binding.to_string(),
            events: events.clone(),
            started,
            worlds,
            human: mode == RecordDialogs::Human,
            open_dialog: None,
        };
        let task = tokio::spawn(observe_session(c, watch, script_id, early, stop_rx));
        self.observers.lock().map_err(|_| poisoned())?.insert(
            target.to_string(),
            Observer {
                events,
                stop,
                start_url: start_url.clone(),
                started,
                task,
            },
        );
        Ok(json!({
            "installed": installed.get("installed").and_then(Value::as_bool).unwrap_or(false),
            "url": start_url,
            "dialogs": mode.as_str(),
        }))
    }

    async fn observe_status(&self, target: &str) -> Result<Value, BrowserError> {
        let m = self.observers.lock().map_err(|_| poisoned())?;
        Ok(match m.get(target) {
            Some(o) => json!({
                "recording": !o.task.is_finished(),
                "event_count": o.events.lock().map(|e| e.len()).unwrap_or(0),
                "elapsed_ms": o.started.elapsed().as_millis() as u64,
            }),
            None => json!({ "recording": false, "event_count": 0 }),
        })
    }

    async fn observe_stop(
        &self,
        target: &str,
        teardown_script: &str,
    ) -> Result<Value, BrowserError> {
        let obs = self
            .observers
            .lock()
            .map_err(|_| poisoned())?
            .remove(target)
            .ok_or_else(|| {
                BrowserError::NotFound(
                    "no recording is in progress for this tab (browser_record start first)".into(),
                )
            })?;
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let mut done = ObserverDone {
            script_removed: false,
            dialogs: Vec::new(),
        };
        if obs
            .stop
            .send((teardown_script.to_string(), reply_tx))
            .is_ok()
        {
            if let Ok(Ok(d)) =
                tokio::time::timeout(tokio::time::Duration::from_secs(5), reply_rx).await
            {
                done = d;
            }
        }
        // Normally finished by now; if the session wedged, do not leave it.
        obs.task.abort();
        let events = obs.events.lock().map(|e| e.clone()).unwrap_or_default();
        Ok(json!({
            "start_url": obs.start_url,
            "events": events,
            "script_removed": done.script_removed,
            "dialogs": done.dialogs,
        }))
    }

    async fn dialog(
        &self,
        target: &str,
        policy: Option<DialogPolicy>,
    ) -> Result<Value, BrowserError> {
        if target.starts_with("safari-") {
            let entry = self.get_safari_session(target)?;
            // Safari WebDriver has no standing dialog policy: it can only answer
            // the alert that is open right now.
            let open = entry.session.alert_text().await?;
            let Some(p) = policy else {
                return Ok(json!({
                    "open": open.is_some(),
                    "message": open,
                    "engine": "webkit"
                }));
            };
            let Some(message) = open else {
                return Err(BrowserError::NotFound(
                    "no dialog is open; Safari WebDriver cannot set a standing dialog policy, \
                     only answer one that is open"
                        .into(),
                ));
            };
            let answered = match p {
                DialogPolicy::Accept(text) => {
                    if let Some(t) = text {
                        entry.session.send_alert_text(&t).await?;
                    }
                    entry.session.accept_alert().await?;
                    "accepted"
                }
                DialogPolicy::Dismiss => {
                    entry.session.dismiss_alert().await?;
                    "dismissed"
                }
            };
            return Ok(json!({
                "handled": true,
                "answered": answered,
                "message": message,
                "engine": "webkit"
            }));
        }

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
        if target.starts_with("safari-") {
            return Err(BrowserError::Unsupported(
                "network interception and monitoring are CDP-specific and not supported by Safari WebDriver (use Chromium for CDP network tooling)".into(),
            ));
        }

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
        if target.starts_with("safari-") {
            let entry = self.get_safari_session(target)?;
            match action {
                "get" => {
                    let cookies = entry.session.get_cookies().await?;
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
                    return Ok(
                        json!({ "cookies": redacted, "count": redacted.len(), "engine": "webkit" }),
                    );
                }
                "set" => {
                    let ck =
                        cookie.ok_or_else(|| BrowserError::Failed("set needs 'cookie'".into()))?;
                    entry.session.add_cookie(&ck).await?;
                    return Ok(json!({ "ok": true, "engine": "webkit" }));
                }
                "clear" => {
                    entry.session.delete_cookies().await?;
                    return Ok(json!({ "ok": true, "cleared": true, "engine": "webkit" }));
                }
                other => {
                    return Err(BrowserError::Failed(format!(
                        "unknown cookies action '{other}'"
                    )))
                }
            }
        }

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
        if target.starts_with("safari-") {
            let entry = self.get_safari_session(target)?;
            match action {
                "start" => {
                    let expr = format!("return {JS_CAPTURE_HOOK};");
                    let now = entry.session.execute_sync(&expr, &[]).await?;
                    return Ok(json!({ "ok": true, "current_page": now, "engine": "webkit" }));
                }
                "clear" => {
                    let expr = "return (function(){if(window.__agentctl){window.__agentctl.net.length=0;window.__agentctl.con.length=0;}return true;})();";
                    entry.session.execute_sync(expr, &[]).await?;
                    return Ok(json!({ "ok": true, "cleared": true, "engine": "webkit" }));
                }
                "read" => {
                    let only_errors = opts
                        .get("only_errors")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let filter = opts.get("filter").and_then(Value::as_str).unwrap_or("");
                    let armed = entry
                        .session
                        .execute_sync("return !!window.__agentctl_installed;", &[])
                        .await?;
                    if armed.as_bool() != Some(true) {
                        return Err(BrowserError::Failed(
                            "capture is not armed on this page; call browser_capture action='start' first".into(),
                        ));
                    }
                    let buf = entry
                        .session
                        .execute_sync(
                            "return JSON.stringify(window.__agentctl||{net:[],con:[]});",
                            &[],
                        )
                        .await?;
                    let parsed: Value = buf
                        .as_str()
                        .and_then(|s| serde_json::from_str(s).ok())
                        .unwrap_or(buf);
                    let empty = vec![];
                    let net = parsed
                        .get("net")
                        .and_then(Value::as_array)
                        .unwrap_or(&empty);
                    let con = parsed
                        .get("con")
                        .and_then(Value::as_array)
                        .unwrap_or(&empty);
                    let keep = |row: &Value, want_bad: bool| -> bool {
                        if !filter.is_empty()
                            && !serde_json::to_string(row)
                                .unwrap_or_default()
                                .contains(filter)
                        {
                            return false;
                        }
                        if want_bad {
                            return row.get("ok").and_then(Value::as_bool) == Some(false);
                        }
                        true
                    };
                    let net: Vec<Value> = net
                        .iter()
                        .filter(|r| keep(r, only_errors))
                        .cloned()
                        .collect();
                    let con: Vec<Value> = con
                        .iter()
                        .filter(|r| {
                            filter.is_empty()
                                || serde_json::to_string(r)
                                    .unwrap_or_default()
                                    .contains(filter)
                        })
                        .cloned()
                        .collect();
                    return Ok(json!({
                        "network": net,
                        "console": con,
                        "network_count": net.len(),
                        "console_count": con.len(),
                        "engine": "webkit"
                    }));
                }
                other => {
                    return Err(BrowserError::Failed(format!(
                        "unknown capture action '{other}'"
                    )))
                }
            }
        }

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
                let only_errors = opts
                    .get("only_errors")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let filter = opts.get("filter").and_then(Value::as_str).unwrap_or("");
                let armed = Self::eval_value(&mut c, "!!window.__agentctl_installed").await?;
                if armed.as_bool() != Some(true) {
                    return Err(BrowserError::Failed(
                        "capture is not armed on this page; call browser_capture action='start' first".into(),
                    ));
                }
                let buf =
                    Self::eval_value(&mut c, "JSON.stringify(window.__agentctl||{net:[],con:[]})")
                        .await?;
                // eval returns the JSON string; parse it back to structured data.
                let parsed: Value = buf
                    .as_str()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or(buf);
                let empty = vec![];
                let net = parsed
                    .get("net")
                    .and_then(Value::as_array)
                    .unwrap_or(&empty);
                let con = parsed
                    .get("con")
                    .and_then(Value::as_array)
                    .unwrap_or(&empty);
                let keep = |row: &Value, want_bad: bool| -> bool {
                    if !filter.is_empty()
                        && !serde_json::to_string(row)
                            .unwrap_or_default()
                            .contains(filter)
                    {
                        return false;
                    }
                    if want_bad {
                        return row.get("ok").and_then(Value::as_bool) == Some(false);
                    }
                    true
                };
                let net: Vec<Value> = net
                    .iter()
                    .filter(|r| keep(r, only_errors))
                    .cloned()
                    .collect();
                let con: Vec<Value> = con
                    .iter()
                    .filter(|r| {
                        filter.is_empty()
                            || serde_json::to_string(r)
                                .unwrap_or_default()
                                .contains(filter)
                    })
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
                settle =
                    Some(json!({ "name": "wait_selector", "ok": false, "detail": berr_msg(&e) }));
            }
        } else if spec.get("wait_dom_settled").and_then(Value::as_bool) == Some(true) {
            if let Err(e) = self.wait(target, "dom_settled", None, timeout).await {
                settle = Some(
                    json!({ "name": "wait_dom_settled", "ok": false, "detail": berr_msg(&e) }),
                );
            }
        } else if spec.get("wait_network_idle").and_then(Value::as_bool) == Some(true) {
            if let Err(e) = self.wait(target, "network_idle", None, timeout).await {
                settle = Some(
                    json!({ "name": "wait_network_idle", "ok": false, "detail": berr_msg(&e) }),
                );
            }
        }
        let is_safari = target.starts_with("safari-");
        let c_opt = if is_safari {
            None
        } else {
            Some(self.conn(target).await?)
        };
        let spec_lit = serde_json::to_string(spec).unwrap_or_else(|_| "{}".into());
        let expr = JS_ASSERT.replace("__SPEC__", &spec_lit);
        let result = if is_safari {
            let entry = self.get_safari_session(target)?;
            entry
                .session
                .execute_sync(&format!("return {expr};"), &[])
                .await?
        } else {
            let mut c = c_opt.unwrap();
            Self::eval_value(&mut c, &expr).await?
        };
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
                "no assertions given (use text/not_text/url/selector/no_console_errors/\
                 no_failed_requests/a11y/style/component)"
                    .into(),
            ));
        }
        Ok(json!({ "passed": passed, "checks": checks }))
    }

    async fn fill_form(
        &self,
        target: &str,
        fields: &Value,
        submit: Option<&Value>,
    ) -> Result<Value, BrowserError> {
        let is_safari = target.starts_with("safari-");
        let mut c_opt = if is_safari {
            None
        } else {
            Some(self.conn(target).await?)
        };
        let fields_json = serde_json::to_string(fields).unwrap_or_else(|_| "[]".into());
        let submit_json = match submit {
            Some(s) => serde_json::to_string(s).unwrap_or_else(|_| "null".into()),
            None => "null".into(),
        };

        let showcase_cfg = self
            .showcase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let (showcase_init, showcase_field, showcase_submit) = if showcase_cfg.enabled {
            let glide_ms = showcase_cfg.glide_ms();
            (
                crate::showcase::JS_SHOWCASE_ENGINE,
                format!(
                    r#"
      try {{
        if(typeof window !== 'undefined' && window.__agentctl_showcase && typeof window.__agentctl_showcase.act === 'function') {{
          var fb = el.getBoundingClientRect();
          var fx = Math.round(fb.left + fb.width / 2);
          var fy = Math.round(fb.top + fb.height / 2);
          await window.__agentctl_showcase.act(fx, fy, 'type', val, {glide_ms}, false, true, el, f.secret === true);
        }}
      }} catch(e) {{}}
"#
                ),
                format!(
                    r#"
      try {{
        if(typeof window !== 'undefined' && window.__agentctl_showcase && typeof window.__agentctl_showcase.act === 'function') {{
          var sb = subEl.getBoundingClientRect();
          var sx = Math.round(sb.left + sb.width / 2);
          var sy = Math.round(sb.top + sb.height / 2);
          await window.__agentctl_showcase.act(sx, sy, 'click', null, {glide_ms}, true, true);
        }}
      }} catch(e) {{}}
"#
                ),
            )
        } else {
            ("", String::new(), String::new())
        };

        let expr = JS_FILL_FORM
            .replace("{JS_XPATH}", JS_XPATH)
            .replace("{JS_SHOWCASE_INIT}", showcase_init)
            .replace("{JS_SHOWCASE_FIELD}", &showcase_field)
            .replace("{JS_SHOWCASE_SUBMIT}", &showcase_submit)
            .replace("__FIELDS__", &fields_json)
            .replace("__SUBMIT__", &submit_json);

        let mut v = if is_safari {
            let entry = self.get_safari_session(target)?;
            entry.session.eval_promise(&expr).await?
        } else {
            let c = c_opt.as_mut().unwrap();
            Self::eval_value(c, &expr).await?
        };
        if v.get("ok").and_then(Value::as_bool) != Some(true) {
            let errs = v.get("errors").and_then(Value::as_array);
            let first_err = errs
                .and_then(|a| a.first())
                .and_then(|e| e.get("error"))
                .and_then(Value::as_str)
                .or_else(|| v.get("error").and_then(Value::as_str))
                .unwrap_or("form fill returned no result");
            return Err(BrowserError::Failed(first_err.to_string()));
        }
        if let Some(ref mut c) = c_opt {
            self.note_dialogs(target, c, &mut v);
        }
        Ok(v)
    }

    async fn extract(
        &self,
        target: &str,
        schema: &Value,
        within: Option<&str>,
    ) -> Result<Value, BrowserError> {
        let schema_json = serde_json::to_string(schema).unwrap_or_else(|_| "{}".into());
        let within_json = match within {
            Some(w) => serde_json::to_string(w).unwrap_or_else(|_| "null".into()),
            None => "null".into(),
        };
        let expr = JS_EXTRACT
            .replace("__SCHEMA__", &schema_json)
            .replace("__WITHIN__", &within_json);

        let v = if target.starts_with("safari-") {
            let entry = self.get_safari_session(target)?;
            entry
                .session
                .execute_sync(&format!("return {expr};"), &[])
                .await?
        } else {
            let mut c = self.conn(target).await?;
            Self::eval_value(&mut c, &expr).await?
        };
        if v.get("ok").and_then(Value::as_bool) == Some(false) {
            let msg = v
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("extract failed");
            return Err(BrowserError::Failed(msg.to_string()));
        }
        Ok(v)
    }

    async fn profile_state(&self, target: &str) -> Result<Value, BrowserError> {
        if target.starts_with("safari-") {
            let entry = self.get_safari_session(target)?;
            let cookies = entry.session.get_cookies().await?;
            let storage_js = r#"return (function(){
                return {
                    localStorage: Object.assign({}, window.localStorage),
                    sessionStorage: Object.assign({}, window.sessionStorage),
                    url: location.href
                };
            })();"#;
            let storage = entry.session.execute_sync(storage_js, &[]).await?;
            let ls = storage.get("localStorage").cloned().unwrap_or(json!({}));
            let ss = storage.get("sessionStorage").cloned().unwrap_or(json!({}));
            let url = storage
                .get("url")
                .and_then(Value::as_str)
                .map(|s| s.to_string());
            return Ok(json!({
                "cookies": cookies,
                "localStorage": ls,
                "sessionStorage": ss,
                "url": url,
                "engine": "webkit"
            }));
        }

        let mut c = self.conn(target).await?;
        c.call("Network.enable", json!({})).await.ok();
        let r = c.call("Network.getCookies", json!({})).await?;
        let empty = vec![];
        let cookies = r.get("cookies").and_then(Value::as_array).unwrap_or(&empty);

        let storage_js = r#"(function(){
            return {
                localStorage: Object.assign({}, window.localStorage),
                sessionStorage: Object.assign({}, window.sessionStorage),
                url: location.href
            };
        })()"#;
        let storage = Self::eval_value(&mut c, storage_js)
            .await
            .unwrap_or(json!({}));
        let ls = storage.get("localStorage").cloned().unwrap_or(json!({}));
        let ss = storage.get("sessionStorage").cloned().unwrap_or(json!({}));
        let url = storage
            .get("url")
            .and_then(Value::as_str)
            .map(|s| s.to_string());

        Ok(json!({
            "cookies": cookies,
            "localStorage": ls,
            "sessionStorage": ss,
            "url": url,
        }))
    }

    async fn profile_restore(&self, target: &str, state: &Value) -> Result<Value, BrowserError> {
        if target.starts_with("safari-") {
            let entry = self.get_safari_session(target)?;
            if let Some(cookies) = state.get("cookies").and_then(Value::as_array) {
                for c in cookies {
                    entry.session.add_cookie(c).await?;
                }
            }
            let ls_json = serde_json::to_string(state.get("localStorage").unwrap_or(&json!({})))
                .unwrap_or_else(|_| "{}".into());
            let ss_json = serde_json::to_string(state.get("sessionStorage").unwrap_or(&json!({})))
                .unwrap_or_else(|_| "{}".into());
            let restore_js = format!(
                r#"return (function(){{
                    var ls = {ls_json};
                    for(var k in ls){{ localStorage.setItem(k, ls[k]); }}
                    var ss = {ss_json};
                    for(var sk in ss){{ sessionStorage.setItem(sk, ss[sk]); }}
                    return {{ ok: true }};
                }})();"#
            );
            // A throwing script (quota, opaque origin) surfaces as an error.
            let r = entry.session.execute_sync(&restore_js, &[]).await?;
            if r.get("ok").and_then(Value::as_bool) != Some(true) {
                return Err(BrowserError::Failed(
                    "storage restore did not complete".into(),
                ));
            }
            return Ok(json!({ "restored": true, "engine": "webkit" }));
        }

        let mut c = self.conn(target).await?;
        c.call("Network.enable", json!({})).await.ok();
        if let Some(cookies) = state.get("cookies").and_then(Value::as_array) {
            let _ = c
                .call("Network.setCookies", json!({ "cookies": cookies }))
                .await;
        }
        let ls_json = serde_json::to_string(state.get("localStorage").unwrap_or(&json!({})))
            .unwrap_or_else(|_| "{}".into());
        let ss_json = serde_json::to_string(state.get("sessionStorage").unwrap_or(&json!({})))
            .unwrap_or_else(|_| "{}".into());
        let restore_js = format!(
            r#"(function(){{
                var ls = {ls_json};
                var ss = {ss_json};
                try {{
                    if(ls){{
                        window.localStorage.clear();
                        for(var k in ls){{ window.localStorage.setItem(k, ls[k]); }}
                    }}
                    if(ss){{
                        window.sessionStorage.clear();
                        for(var k in ss){{ window.sessionStorage.setItem(k, ss[k]); }}
                    }}
                    return {{ ok: true }};
                }} catch(e) {{
                    return {{ ok: false, error: String(e) }};
                }}
            }})()"#
        );
        let v = Self::eval_value(&mut c, &restore_js).await?;
        Ok(v)
    }

    async fn branch_create(&self, target_id: &str, branch_id: &str) -> Result<Value, BrowserError> {
        let b_id = branch_id.trim();
        if b_id.is_empty() {
            return Err(BrowserError::Failed("branch_id must not be empty".into()));
        }
        // Reject duplicates and over-cap requests before allocating anything.
        {
            let mgr = self
                .branches
                .lock()
                .map_err(|_| BrowserError::Failed("branches mutex poisoned".into()))?;
            mgr.check_capacity(b_id).map_err(branch_err)?;
        }

        // 1. Locate owning browser and its browser WebSocket
        let (b, browser_ws) = self.browser_ws_for_target(target_id).await?;

        // 2. Capture parent state (cookies, storage, url)
        let parent_state = self.profile_state(target_id).await?;
        let url = parent_state
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("about:blank")
            .to_string();

        // 3. Connect to top-level browser CDP
        let mut b_conn = CdpConn::connect(&browser_ws).await?;

        // 4. Create an isolated browser context. There is deliberately no
        // fallback to the shared default context: a "branch" that shares
        // cookies and storage with its parent is not isolated, and reporting
        // it as such would be a lie.
        let ctx_id = b_conn
            .call("Target.createBrowserContext", json!({}))
            .await
            .map_err(|e| {
                ctx_err(
                    e,
                    "cannot create an isolated browser context for the branch",
                )
            })?
            .get("browserContextId")
            .and_then(Value::as_str)
            .map(|s| s.to_string())
            .ok_or_else(|| {
                BrowserError::Failed(
                    "Target.createBrowserContext returned no browserContextId".into(),
                )
            })?;

        // 5. Create the new target in that context; dispose the context again
        // if that fails so nothing leaks.
        let created = b_conn
            .call(
                "Target.createTarget",
                json!({ "url": url, "browserContextId": ctx_id }),
            )
            .await;
        let branch_target_id = match created {
            Ok(r) => match r.get("targetId").and_then(Value::as_str) {
                Some(t) => t.to_string(),
                None => {
                    let _ = b_conn
                        .call(
                            "Target.disposeBrowserContext",
                            json!({ "browserContextId": ctx_id }),
                        )
                        .await;
                    return Err(BrowserError::Failed(
                        "no targetId in createTarget response".into(),
                    ));
                }
            },
            Err(e) => {
                let _ = b_conn
                    .call(
                        "Target.disposeBrowserContext",
                        json!({ "browserContextId": ctx_id }),
                    )
                    .await;
                return Err(ctx_err(e, "cannot create the branch tab"));
            }
        };

        let now = now_ms();
        let branch = crate::branch::Branch {
            branch_id: b_id.to_string(),
            parent_target_id: target_id.to_string(),
            branch_target_id: branch_target_id.clone(),
            browser_context_id: Some(ctx_id.clone()),
            browser_host: b.host.clone(),
            browser_port: b.port,
            initial_url: url.clone(),
            status: crate::branch::BranchStatus::Active,
            created_at_ms: now,
        };

        // 6. Register before the (fallible, slow) load/restore so a concurrent
        // create past the cap is rejected here, and clean up if it is.
        let inserted = {
            let mut mgr = self
                .branches
                .lock()
                .map_err(|_| BrowserError::Failed("branches mutex poisoned".into()))?;
            mgr.insert(branch.clone())
        };
        if let Err(e) = inserted {
            let _ = teardown_branch(&branch).await;
            return Err(branch_err(e));
        }

        // 7. Wait for the branch tab to reach the parent's URL and finish
        // loading; the storage restore below needs a real origin.
        if url != "about:blank" && !url.is_empty() {
            let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
            while tokio::time::Instant::now() < deadline {
                if let Ok(mut c) = self.conn(&branch_target_id).await {
                    if let Ok(v) =
                        Self::eval_value(&mut c, "location.href + '|' + document.readyState").await
                    {
                        if let Some((cur, ready)) = v.as_str().and_then(|s| s.rsplit_once('|')) {
                            if cur == url && (ready == "interactive" || ready == "complete") {
                                break;
                            }
                        }
                    }
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
            }
        }

        // 8. Inject parent cookies and storage into the branch target and say
        // honestly whether that worked.
        let restore = self
            .profile_restore(&branch_target_id, &parent_state)
            .await
            .and_then(|v| restore_ok(&v).map(|()| v));
        let (state_restored, restore_error) = match restore {
            Ok(_) => (true, None),
            Err(e) => {
                let m = err_msg(&e);
                tracing::warn!("could not restore parent state into branch target: {m}");
                (false, Some(m))
            }
        };

        Ok(json!({
            "created": true,
            "branch_id": b_id,
            "parent_target_id": target_id,
            "branch_target_id": branch_target_id,
            "url": url,
            "isolated_context": true,
            "state_restored": state_restored,
            "state_restore_error": restore_error
        }))
    }

    async fn branch_commit(&self, branch_id: &str) -> Result<Value, BrowserError> {
        // Do the work first; only mark committed once every step succeeded, so
        // a failure leaves the branch Active and retryable.
        let branch = {
            let mgr = self
                .branches
                .lock()
                .map_err(|_| BrowserError::Failed("branches mutex poisoned".into()))?;
            mgr.ensure_active(branch_id).map_err(branch_err)?
        };

        // 1. Snapshot branch's final state (url, cookies, storage).
        let branch_state = self
            .profile_state(&branch.branch_target_id)
            .await
            .map_err(|e| ctx_err(e, "cannot read the branch's state"))?;
        let final_url = branch_state
            .get("url")
            .and_then(Value::as_str)
            .filter(|u| !u.is_empty())
            .ok_or_else(|| {
                BrowserError::Failed(
                    "cannot read the branch's URL (is the branch tab still open?)".into(),
                )
            })?
            .to_string();

        // 2. Move the parent to the branch's URL and wait for that load to
        // finish *before* restoring storage, which is origin-scoped.
        let blank = final_url == "about:blank";
        if !blank {
            self.goto_and_wait(&branch.parent_target_id, &final_url, 15_000)
                .await
                .map_err(|e| ctx_err(e, "cannot navigate the parent tab to the branch URL"))?;
            let restored = self
                .profile_restore(&branch.parent_target_id, &branch_state)
                .await
                .map_err(|e| ctx_err(e, "cannot restore branch state into the parent"))?;
            restore_ok(&restored)
                .map_err(|e| ctx_err(e, "cannot restore branch state into the parent"))?;
        }

        // 3. Tear the branch down. If that fails the parent already has the
        // state, but the branch tab is still open: stay Active and say so.
        teardown_branch(&branch).await.map_err(|e| {
            ctx_err(
                e,
                "the branch state was applied to the parent but the branch tab could not be \
                 closed; call branch_discard to retry cleanup",
            )
        })?;

        {
            let mut mgr = self
                .branches
                .lock()
                .map_err(|_| BrowserError::Failed("branches mutex poisoned".into()))?;
            mgr.mark_committed(branch_id).map_err(branch_err)?;
        }

        Ok(json!({
            "committed": true,
            "branch_id": branch_id,
            "parent_target_id": branch.parent_target_id,
            "final_url": final_url,
            "storage_restored": !blank,
            "branch_closed": true
        }))
    }

    async fn branch_discard(&self, branch_id: &str) -> Result<Value, BrowserError> {
        let branch = {
            let mgr = self
                .branches
                .lock()
                .map_err(|_| BrowserError::Failed("branches mutex poisoned".into()))?;
            mgr.ensure_active(branch_id).map_err(branch_err)?
        };

        // Actually close the tab and dispose the context; report failure
        // instead of pretending, and keep the branch Active so it can be retried.
        teardown_branch(&branch)
            .await
            .map_err(|e| ctx_err(e, "could not close the branch tab/context"))?;

        {
            let mut mgr = self
                .branches
                .lock()
                .map_err(|_| BrowserError::Failed("branches mutex poisoned".into()))?;
            mgr.mark_discarded(branch_id).map_err(branch_err)?;
        }

        Ok(json!({
            "discarded": true,
            "branch_id": branch_id,
            "branch_closed": true
        }))
    }

    async fn branch_switch(&self, branch_id: &str) -> Result<Value, BrowserError> {
        let branch = {
            let mgr = self
                .branches
                .lock()
                .map_err(|_| BrowserError::Failed("branches mutex poisoned".into()))?;
            mgr.get(branch_id)
                .cloned()
                .ok_or_else(|| BrowserError::NotFound(format!("branch '{branch_id}' not found")))?
        };

        if branch.status != crate::branch::BranchStatus::Active {
            return Err(BrowserError::Failed(format!(
                "branch '{branch_id}' is {:?}, cannot switch",
                branch.status
            )));
        }

        let _ = http_json(
            &branch.browser_host,
            branch.browser_port,
            "GET",
            &format!("/json/activate/{}", branch.branch_target_id),
        )
        .await;

        Ok(json!({
            "switched": true,
            "branch_id": branch_id,
            "target_id": branch.branch_target_id
        }))
    }

    async fn branch_list(&self, target_id: Option<&str>) -> Result<Value, BrowserError> {
        let mgr = self
            .branches
            .lock()
            .map_err(|_| BrowserError::Failed("branches mutex poisoned".into()))?;
        let list = mgr.list(target_id);
        let count = list.len();
        Ok(json!({
            "branches": list,
            "count": count
        }))
    }

    async fn checkpoint_save(
        &self,
        target_id: &str,
        tag: Option<&str>,
    ) -> Result<Value, BrowserError> {
        let mut c = self.conn(target_id).await?;
        c.call("Network.enable", json!({})).await.ok();
        let r = c.call("Network.getCookies", json!({})).await?;
        let empty = vec![];
        let cookies = r
            .get("cookies")
            .and_then(Value::as_array)
            .unwrap_or(&empty)
            .clone();

        let capture_js = format!(
            r#"(function(){{
  {JS_XPATH}
  var warnings = [];
  var inputs = [];
  var els = document.querySelectorAll('input, textarea, select');
  for (var i = 0; i < els.length; i++) {{
    var el = els[i];
    var type = (el.type || '').toLowerCase();
    // File inputs cannot be set from script and hold only a fake path.
    if (type === 'file') continue;
    try {{
      inputs.push({{
        id: el.id || null,
        name: el.name || null,
        tag: el.tagName.toLowerCase(),
        input_type: type,
        value: el.value,
        checked: !!el.checked,
        selected_index: typeof el.selectedIndex === 'number' ? el.selectedIndex : -1,
        xpath: (typeof __xp === 'function') ? __xp(el) : null
      }});
    }} catch (e) {{ warnings.push('input ' + (el.id || el.name || i) + ': ' + String(e)); }}
  }}
  function readStore(name) {{
    try {{ return Object.assign({{}}, window[name]); }}
    catch (e) {{ warnings.push(name + ' unreadable: ' + String(e)); return {{}}; }}
  }}
  return {{
    url: location.href,
    title: document.title,
    scroll_x: window.scrollX || 0,
    scroll_y: window.scrollY || 0,
    local_storage: readStore('localStorage'),
    session_storage: readStore('sessionStorage'),
    inputs: inputs,
    warnings: warnings
  }};
}})()"#
        );

        // A failed capture is an error, not an empty checkpoint.
        let snapshot = Self::eval_value(&mut c, &capture_js)
            .await
            .map_err(|e| ctx_err(e, "checkpoint could not capture page state"))?;
        let warnings = snapshot.get("warnings").cloned().unwrap_or(json!([]));
        let url = snapshot
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let title = snapshot
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let scroll_x = snapshot
            .get("scroll_x")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let scroll_y = snapshot
            .get("scroll_y")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let local_storage = snapshot.get("local_storage").cloned().unwrap_or(json!({}));
        let session_storage = snapshot
            .get("session_storage")
            .cloned()
            .unwrap_or(json!({}));
        let raw_inputs = snapshot
            .get("inputs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let inputs: Vec<crate::checkpoint::FormInputState> = raw_inputs
            .into_iter()
            .filter_map(|v| serde_json::from_value(v).ok())
            .collect();

        let now = now_ms();
        let checkpoint_tag = tag
            .filter(|t| !t.trim().is_empty())
            .map(String::from)
            .unwrap_or_else(|| format!("chk_{now}"));

        let cp = crate::checkpoint::Checkpoint {
            tag: checkpoint_tag.clone(),
            target_id: target_id.to_string(),
            url: url.clone(),
            title: title.clone(),
            timestamp_ms: now,
            cookies: cookies.clone(),
            local_storage,
            session_storage,
            scroll_x,
            scroll_y,
            inputs: inputs.clone(),
        };

        {
            let mut store = self
                .checkpoints
                .lock()
                .map_err(|_| BrowserError::Failed("checkpoints mutex poisoned".into()))?;
            store.save(cp);
        }

        Ok(json!({
            "saved": true,
            "tag": checkpoint_tag,
            "target_id": target_id,
            "url": url,
            "title": title,
            "inputs_captured": inputs.len(),
            "cookies_captured": cookies.len(),
            "warnings": warnings,
            "timestamp_ms": now
        }))
    }

    async fn checkpoint_rollback(
        &self,
        target_id: &str,
        tag: Option<&str>,
    ) -> Result<Value, BrowserError> {
        let cp = {
            let store = self
                .checkpoints
                .lock()
                .map_err(|_| BrowserError::Failed("checkpoints mutex poisoned".into()))?;
            store.get(target_id, tag).cloned().ok_or_else(|| {
                BrowserError::NotFound(format!(
                    "checkpoint '{tag:?}' not found for target '{target_id}'"
                ))
            })?
        };

        let start = std::time::Instant::now();
        let mut c = self.conn(target_id).await?;

        // 1. Cookies first, so the page being restored loads with them. A
        // failure is an error: a rollback that silently keeps the wrong
        // session is worse than one that says it could not.
        c.call("Network.enable", json!({})).await.ok();
        if !cp.cookies.is_empty() {
            c.call("Network.setCookies", json!({ "cookies": cp.cookies }))
                .await
                .map_err(|e| ctx_err(e, "rollback could not restore cookies"))?;
        }

        // 2. Navigate back if needed, and wait for the new document to load
        // before touching storage or the DOM (a fixed sleep raced the load).
        let cur_url_val = Self::eval_value(&mut c, "location.href")
            .await
            .unwrap_or(Value::Null);
        let cur_url = cur_url_val.as_str().unwrap_or("");
        let navigated = !cp.url.is_empty() && cur_url != cp.url;
        if navigated {
            self.goto_and_wait(target_id, &cp.url, 15_000)
                .await
                .map_err(|e| ctx_err(e, "rollback could not navigate back to the checkpoint"))?;
        }

        // 3. Restore storage, input values, radio/checkbox checks, select
        // indexes and scroll. Every field is its own try/catch so one bad
        // field cannot silently abort the rest, and the result is counted.
        let ls_json = serde_json::to_string(&cp.local_storage).unwrap_or_else(|_| "{}".into());
        let ss_json = serde_json::to_string(&cp.session_storage).unwrap_or_else(|_| "{}".into());
        let inputs_json = serde_json::to_string(&cp.inputs).unwrap_or_else(|_| "[]".into());
        let sx = cp.scroll_x;
        let sy = cp.scroll_y;

        let restore_js = format!(
            r#"(function(ls, ss, inputs, sx, sy){{
  var out = {{ ok: true, restored_inputs: 0, missing_inputs: 0, skipped_file_inputs: 0, errors: [] }};
  function restoreStore(name, data) {{
    data = data || {{}};
    var s = null;
    try {{ s = window[name]; }} catch (e) {{ s = null; }}
    if (!s) {{
      // Opaque origins (about:blank) have no storage; only an error if the
      // checkpoint actually held something to put back.
      if (Object.keys(data).length) out.errors.push(name + ' is not available on this page');
      return;
    }}
    try {{
      s.clear();
      for (var k in data) {{ s.setItem(k, data[k]); }}
    }} catch (e) {{ out.errors.push(name + ': ' + String(e)); }}
  }}
  restoreStore('localStorage', ls);
  restoreStore('sessionStorage', ss);
  for (var i = 0; i < (inputs || []).length; i++) {{
    var item = inputs[i];
    var label = item.id || item.name || item.xpath || ('#' + i);
    try {{
      if (item.input_type === 'file') {{ out.skipped_file_inputs++; continue; }}
      var el = null;
      if (item.id) {{ el = document.getElementById(item.id); }}
      if (!el && item.xpath) {{
        try {{
          el = document.evaluate(item.xpath, document, null, XPathResult.FIRST_ORDERED_NODE_TYPE, null).singleNodeValue;
        }} catch (e) {{ el = null; }}
      }}
      if (!el && item.name) {{
        try {{ el = document.querySelector('[name="' + CSS.escape(item.name) + '"]'); }} catch (e) {{ el = null; }}
      }}
      if (!el) {{ out.missing_inputs++; continue; }}
      if (item.tag === 'select') {{
        if (item.selected_index >= 0) el.selectedIndex = item.selected_index;
      }} else if (item.input_type === 'checkbox' || item.input_type === 'radio') {{
        el.checked = !!item.checked;
      }} else if (item.value !== undefined && item.value !== null) {{
        el.value = typeof item.value === 'string' ? item.value : JSON.stringify(item.value);
      }}
      el.dispatchEvent(new Event('input', {{ bubbles: true }}));
      el.dispatchEvent(new Event('change', {{ bubbles: true }}));
      out.restored_inputs++;
    }} catch (e) {{ out.errors.push('input ' + label + ': ' + String(e)); }}
  }}
  try {{
    if (typeof sx === 'number' && typeof sy === 'number') window.scrollTo(sx, sy);
  }} catch (e) {{ out.errors.push('scroll: ' + String(e)); }}
  out.ok = out.errors.length === 0;
  return out;
}})({ls_json}, {ss_json}, {inputs_json}, {sx}, {sy})"#
        );

        let res = Self::eval_value(&mut c, &restore_js)
            .await
            .map_err(|e| ctx_err(e, "rollback could not restore page state"))?;
        let count = |k: &str| res.get(k).and_then(Value::as_u64).unwrap_or(0);
        let restored_count = count("restored_inputs");
        let missing_inputs = count("missing_inputs");
        let skipped_file_inputs = count("skipped_file_inputs");
        let errors: Vec<String> = res
            .get("errors")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        if res.get("ok").and_then(Value::as_bool) != Some(true) || !errors.is_empty() {
            return Err(BrowserError::Failed(format!(
                "rollback to '{}' was incomplete ({} inputs restored, {} missing): {}",
                cp.tag,
                restored_count,
                missing_inputs,
                if errors.is_empty() {
                    "restore script returned no result".to_string()
                } else {
                    errors.join("; ")
                }
            )));
        }
        let duration_ms = start.elapsed().as_millis() as u64;

        Ok(json!({
            "rolled_back": true,
            "tag": cp.tag,
            "target_id": target_id,
            "url": cp.url,
            "title": cp.title,
            "navigated": navigated,
            "cookies_restored": cp.cookies.len(),
            "restored_inputs": restored_count,
            "missing_inputs": missing_inputs,
            "skipped_file_inputs": skipped_file_inputs,
            "complete": missing_inputs == 0,
            "duration_ms": duration_ms,
            "timestamp_ms": cp.timestamp_ms
        }))
    }

    async fn checkpoint_list(&self, target_id: Option<&str>) -> Result<Value, BrowserError> {
        let store = self
            .checkpoints
            .lock()
            .map_err(|_| BrowserError::Failed("checkpoints mutex poisoned".into()))?;
        let list: Vec<Value> = store
            .list(target_id)
            .into_iter()
            .map(|cp| {
                json!({
                    "tag": cp.tag,
                    "target_id": cp.target_id,
                    "url": cp.url,
                    "title": cp.title,
                    "timestamp_ms": cp.timestamp_ms,
                    "inputs_count": cp.inputs.len(),
                    "cookies_count": cp.cookies.len()
                })
            })
            .collect();
        let count = list.len();
        Ok(json!({
            "checkpoints": list,
            "count": count
        }))
    }

    async fn checkpoint_delete(
        &self,
        target_id: &str,
        tag: Option<&str>,
    ) -> Result<Value, BrowserError> {
        let count = {
            let mut store = self
                .checkpoints
                .lock()
                .map_err(|_| BrowserError::Failed("checkpoints mutex poisoned".into()))?;
            store.delete(target_id, tag)
        };
        Ok(json!({
            "deleted": count,
            "target_id": target_id
        }))
    }

    async fn showcase(
        &self,
        target: &str,
        config: Option<crate::showcase::ShowcaseConfig>,
    ) -> Result<Value, BrowserError> {
        if let Some(cfg) = config {
            *self
                .showcase
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = cfg;
        }
        let cfg = self
            .showcase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if !cfg.enabled {
            self.teardown_showcase(None).await;
        }
        if cfg.enabled && !target.is_empty() {
            let is_safari = target.starts_with("safari-");
            let init_script = format!(
                r#"(function(){{ {} return true; }})()"#,
                crate::showcase::JS_SHOWCASE_ENGINE
            );
            if is_safari {
                if let Ok(entry) = self.get_safari_session(target) {
                    let _ = entry
                        .session
                        .execute_sync(&format!("return {init_script};"), &[])
                        .await;
                }
            } else if let Ok(mut c) = self.conn(target).await {
                let _ = Self::eval_value(&mut c, &init_script).await;
            }
        }
        Ok(cfg.to_json())
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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

/// Common macOS/Linux locations for a Chromium-family browser, tried in order.
/// The engine speaks the Chrome DevTools Protocol, so any of these (Chrome,
/// Chromium, Edge, Brave, Vivaldi, Opera) works; Firefox and Safari do not
/// speak CDP and are not launchable here.
pub const CHROME_BINS: &[&str] = &[
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
    "/usr/bin/google-chrome",
    "/usr/bin/google-chrome-stable",
    "/usr/bin/chromium",
    "/usr/bin/chromium-browser",
    "/snap/bin/chromium",
    "/usr/bin/microsoft-edge",
    "/usr/bin/microsoft-edge-stable",
    "/usr/bin/brave-browser",
    "/usr/bin/brave",
    "/usr/bin/vivaldi",
    "/usr/bin/vivaldi-stable",
    "/usr/bin/opera",
];

/// Browser binaries to try, in priority order: the native locations first,
/// then the flatpak exported wrappers. A flatpak export is a tiny shell script
/// that `exec`s `flatpak run <app> "$@"`, so it forwards our Chrome flags
/// verbatim and can be launched exactly like a native binary. Including it
/// means a Chrome installed only as a flatpak (common on Fedora and other
/// distros) is launchable without the operator pre-starting it by hand.
fn browser_bin_candidates() -> Vec<String> {
    // The flatpak app ids whose exported wrappers we can exec directly.
    const FLATPAK_APPS: &[&str] = &[
        "com.google.Chrome",
        "org.chromium.Chromium",
        "com.microsoft.Edge",
        "com.brave.Browser",
    ];
    let mut v: Vec<String> = CHROME_BINS.iter().map(|s| (*s).to_string()).collect();
    for app in FLATPAK_APPS {
        v.push(format!("/var/lib/flatpak/exports/bin/{app}"));
    }
    if let Ok(home) = std::env::var("HOME") {
        for app in FLATPAK_APPS {
            v.push(format!("{home}/.local/share/flatpak/exports/bin/{app}"));
        }
    }
    v
}

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

/// Ask a launched browser to quit itself over CDP (`Browser.close`).
///
/// This is the reliable way to stop a browser we launched. A native Chrome
/// exits when we kill the child we hold, but a flatpak or snap Chrome is run
/// behind a launcher and re-parents its real processes (via `zypak`/`bwrap`)
/// out of our process group, so neither `child.kill()` nor a process-group
/// signal reaches them. Telling the browser to close itself does: it tears
/// down its own tree wherever it lives. Best-effort: a browser that never came
/// up, or has already gone, simply is not there to answer.
async fn browser_close(host: &str, port: u16) -> Result<(), BrowserError> {
    let ver = http_json(host, port, "GET", "/json/version").await?;
    let ws = ver
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .ok_or_else(|| BrowserError::Failed("no browser webSocketDebuggerUrl".into()))?;
    let mut conn = CdpConn::connect(ws).await?;
    // The browser may drop the socket as it exits; a send that lands is enough,
    // so a read error on the reply is not a failure.
    let _ = conn.call("Browser.close", json!({})).await;
    Ok(())
}

/// Synchronous `browser_close` for the sync reap paths (`shutdown`, `Drop`).
///
/// Runs the async close on a throwaway current-thread runtime on a fresh
/// thread, which keeps it safe to call whether or not an outer Tokio runtime is
/// active (`block_on` inside a runtime thread would panic). Fully best-effort.
fn browser_close_blocking(host: &str, port: u16) {
    let (h, p) = (host.to_string(), port);
    let _ = std::thread::spawn(move || {
        if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            let _ = rt.block_on(browser_close(&h, p));
        }
    })
    .join();
}

/// Message text of a [`BrowserError`], whatever its kind.
fn err_msg(e: &BrowserError) -> String {
    match e {
        BrowserError::PermissionDenied(m)
        | BrowserError::NotFound(m)
        | BrowserError::Unsupported(m)
        | BrowserError::Timeout(m)
        | BrowserError::Failed(m) => m.clone(),
    }
}

/// Prefix a [`BrowserError`]'s message with what was being attempted, keeping
/// its kind (so a not-found stays not-found).
fn ctx_err(e: BrowserError, what: &str) -> BrowserError {
    let m = format!("{what}: {}", err_msg(&e));
    match e {
        BrowserError::PermissionDenied(_) => BrowserError::PermissionDenied(m),
        BrowserError::NotFound(_) => BrowserError::NotFound(m),
        BrowserError::Unsupported(_) => BrowserError::Unsupported(m),
        BrowserError::Timeout(_) => BrowserError::Timeout(m),
        BrowserError::Failed(_) => BrowserError::Failed(m),
    }
}

fn branch_err(e: crate::branch::BranchError) -> BrowserError {
    match e {
        crate::branch::BranchError::NotFound(_) => BrowserError::NotFound(e.to_string()),
        _ => BrowserError::Failed(e.to_string()),
    }
}

/// `profile_restore` reports a failed storage write as `{ok:false,error}` in
/// an otherwise successful reply; turn that into an error.
fn restore_ok(v: &Value) -> Result<(), BrowserError> {
    if v.get("ok").and_then(Value::as_bool) == Some(false) {
        let m = v
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("storage restore failed");
        return Err(BrowserError::Failed(m.to_string()));
    }
    Ok(())
}

/// Close a branch's tab and dispose its browser context, then verify the tab
/// is really gone from the browser's target list. Errors if it is not.
async fn teardown_branch(b: &crate::branch::Branch) -> Result<(), BrowserError> {
    let ver = http_json(&b.browser_host, b.browser_port, "GET", "/json/version").await?;
    let ws = ver
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .ok_or_else(|| BrowserError::Failed("no browser webSocketDebuggerUrl".into()))?;
    let mut c = CdpConn::connect(ws).await?;
    let mut problems: Vec<String> = Vec::new();
    // Closing the target may report "No target with given id" when it is
    // already gone, which is the state we want.
    if let Err(e) = c
        .call(
            "Target.closeTarget",
            json!({ "targetId": b.branch_target_id }),
        )
        .await
    {
        let m = err_msg(&e);
        if !m.contains("No target") {
            problems.push(m);
        }
    }
    if let Some(ref cid) = b.browser_context_id {
        if let Err(e) = c
            .call(
                "Target.disposeBrowserContext",
                json!({ "browserContextId": cid }),
            )
            .await
        {
            problems.push(err_msg(&e));
        }
    }
    // Verify rather than trust the replies.
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(3);
    loop {
        let list = http_json(&b.browser_host, b.browser_port, "GET", "/json/list").await?;
        let still_open = list.as_array().is_some_and(|a| {
            a.iter()
                .any(|t| t.get("id").and_then(Value::as_str) == Some(b.branch_target_id.as_str()))
        });
        if !still_open {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            problems.push(format!(
                "target {} is still open after close",
                b.branch_target_id
            ));
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(BrowserError::Failed(problems.join("; ")))
    }
}

/// Blocking [`teardown_branch`] for the sync shutdown path, over every branch
/// at once. Runs on its own thread and runtime for the same reason as
/// [`browser_close_blocking`]. The branches close concurrently under one
/// deadline, so a browser that stopped answering delays exit by that deadline
/// once, not once per branch. Returns each branch that failed, with why.
fn teardown_branches_blocking(branches: Vec<crate::branch::Branch>) -> Vec<(String, BrowserError)> {
    if branches.is_empty() {
        return Vec::new();
    }
    let ids: Vec<String> = branches.iter().map(|b| b.branch_id.clone()).collect();
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                return ids
                    .into_iter()
                    .map(|id| (id, BrowserError::Failed(e.to_string())))
                    .collect();
            }
        };
        rt.block_on(async move {
            let mut set = tokio::task::JoinSet::new();
            for b in branches {
                set.spawn(async move { (b.branch_id.clone(), teardown_branch(&b).await) });
            }
            let mut failed = Vec::new();
            let mut done = std::collections::HashSet::new();
            let deadline = tokio::time::sleep(std::time::Duration::from_secs(8));
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    next = set.join_next() => match next {
                        Some(Ok((id, res))) => {
                            done.insert(id.clone());
                            if let Err(e) = res {
                                failed.push((id, e));
                            }
                        }
                        Some(Err(_)) => {}
                        None => break,
                    },
                    _ = &mut deadline => {
                        set.abort_all();
                        break;
                    }
                }
            }
            for id in ids {
                if !done.contains(&id) && !failed.iter().any(|(f, _)| *f == id) {
                    failed.push((
                        id,
                        BrowserError::Timeout("branch teardown timed out".into()),
                    ));
                }
            }
            failed
        })
    })
    .join()
    .unwrap_or_else(|_| Vec::new())
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

/// Pick a currently-free loopback TCP port by binding `:0` and reading back the
/// assigned port, then releasing it. There is a small window between release and
/// Chrome binding it, but each launch getting its own port is what matters: a
/// fixed port collides when two launches overlap, or when one browser is still
/// shutting down (it holds the port a moment after `Browser.close` returns), and
/// the next launch then attaches to the dying browser instead of its own.
fn free_port() -> Result<u16, BrowserError> {
    let l = std::net::TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| BrowserError::Failed(format!("could not pick a free port: {e}")))?;
    l.local_addr()
        .map(|a| a.port())
        .map_err(|e| BrowserError::Failed(format!("could not read chosen port: {e}")))
}

async fn launch_browser(spec: &Value) -> Result<Launch, BrowserError> {
    use tokio::time::{sleep, Duration};
    // An explicit port is honoured (e.g. to attach DevTools by hand); otherwise
    // each launch gets its own free port so back-to-back launches never collide.
    let port = match spec.get("port").and_then(Value::as_u64) {
        Some(p) => p as u16,
        None => free_port()?,
    };
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
    let candidates = browser_bin_candidates();
    let bin = candidates
        .iter()
        .find(|p| std::path::Path::new(p).exists())
        .ok_or_else(|| {
            BrowserError::NotFound(
                "no Chromium binary found (looked for native Chrome/Chromium and flatpak \
                 com.google.Chrome); attach to a running browser instead"
                    .into(),
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

    #[test]
    fn navigation_wait_verdict_tells_the_old_document_from_the_new() {
        use std::time::Duration;
        let ms = Duration::from_millis;
        // Still the marked (old) document: keep waiting, however loaded it looks.
        assert_eq!(
            nav_probe_verdict(Some("t1"), "complete", "t1", true, ms(5)),
            None
        );
        // New document, still loading: keep waiting.
        assert_eq!(nav_probe_verdict(None, "loading", "t1", true, ms(5)), None);
        assert_eq!(
            nav_probe_verdict(None, "interactive", "t1", true, ms(5)),
            None
        );
        // New document, loaded: done, and it did navigate. A different marker
        // (a page that set its own) is also a different document.
        assert_eq!(
            nav_probe_verdict(None, "complete", "t1", true, ms(5)),
            Some(true)
        );
        assert_eq!(
            nav_probe_verdict(Some("t0"), "complete", "t1", false, ms(5)),
            Some(true)
        );
        // goto/reload always navigate: never give up on the marked document.
        assert_eq!(
            nav_probe_verdict(Some("t1"), "complete", "t1", true, ms(NAV_EXPECT_MS * 10)),
            None
        );
        // A click might not: within the grace keep waiting, after it settle
        // on the loaded page and say nothing navigated.
        assert_eq!(
            nav_probe_verdict(Some("t1"), "complete", "t1", false, ms(NAV_EXPECT_MS - 1)),
            None
        );
        assert_eq!(
            nav_probe_verdict(Some("t1"), "complete", "t1", false, ms(NAV_EXPECT_MS)),
            Some(false)
        );
        // ...but never while the marked document is still loading.
        assert_eq!(
            nav_probe_verdict(Some("t1"), "loading", "t1", false, ms(NAV_EXPECT_MS * 5)),
            None
        );
    }

    #[test]
    fn nav_tokens_are_unique() {
        let a = new_nav_token();
        let b = new_nav_token();
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_digit() || c == '-'), "{a}");
    }

    #[test]
    fn key_event_spec_maps_supported_keys_only() {
        assert_eq!(key_event_spec("Enter").unwrap().vk, 13);
        assert_eq!(key_event_spec("Enter").unwrap().text, Some("\r"));
        assert_eq!(key_event_spec("Escape").unwrap().vk, 27);
        assert_eq!(key_event_spec("Tab").unwrap().vk, 9);
        assert!(key_event_spec("F13").is_none());
        assert!(key_event_spec("").is_none());
    }

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

    /// Discovery lists native locations first, then the flatpak wrappers, and
    /// folds `$HOME` into the per-user flatpak path so a Chrome installed only
    /// as a flatpak is still found.
    #[test]
    fn candidates_include_native_then_flatpak() {
        std::env::set_var("HOME", "/home/tester");
        let c = browser_bin_candidates();
        // native paths come from the const, in order, at the front.
        assert_eq!(c[0], CHROME_BINS[0]);
        assert!(c.iter().any(|p| p == "/usr/bin/google-chrome"));
        // flatpak system wrapper is present and lands after the natives.
        let sys = "/var/lib/flatpak/exports/bin/com.google.Chrome";
        let (sys_i, nat_i) = (
            c.iter()
                .position(|p| p == sys)
                .expect("system flatpak path"),
            c.iter()
                .position(|p| p == "/usr/bin/google-chrome")
                .unwrap(),
        );
        assert!(sys_i > nat_i, "flatpak is a fallback, tried after natives");
        // the per-user path is built from HOME.
        assert!(c
            .iter()
            .any(|p| p == "/home/tester/.local/share/flatpak/exports/bin/com.google.Chrome"));
    }

    /// A picked port is real and usable: nonzero, and free right after (the
    /// listener is dropped), so Chrome can bind it. Two picks in a row should
    /// differ, which is the whole point of not using a fixed port.
    #[test]
    fn free_port_is_usable_and_varies() {
        let a = free_port().expect("pick a port");
        assert!(a != 0, "a real port was chosen");
        // Bindable again now that free_port released it.
        let l = std::net::TcpListener::bind(("127.0.0.1", a)).expect("port is free to bind");
        drop(l);
        let b = free_port().expect("pick another port");
        assert_ne!(a, b, "successive picks are distinct");
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
