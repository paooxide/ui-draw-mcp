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

/// Options for [`BrowserBackend::eval_with`].
#[derive(Debug, Clone, Copy, Default)]
pub struct EvalOptions {
    /// How long to let the script run before giving up (clamped to
    /// [`EVAL_TIMEOUT_MIN_MS`]..=[`EVAL_TIMEOUT_MAX_MS`]); `None` is
    /// [`EVAL_TIMEOUT_DEFAULT_MS`].
    pub timeout_ms: Option<u64>,
    /// Start the script and return at once, without waiting for it or its
    /// result.
    pub detached: bool,
}

/// Default, floor and ceiling for `browser_eval`'s `timeout_ms`. The ceiling
/// sits well under what the transport would wait for on its own.
pub const EVAL_TIMEOUT_DEFAULT_MS: u64 = 10_000;
pub const EVAL_TIMEOUT_MIN_MS: u64 = 100;
pub const EVAL_TIMEOUT_MAX_MS: u64 = 60_000;

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

/// How `act` brings the element into view before acting on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrollMode {
    /// Do not scroll (the element may be off screen; a click still lands).
    None,
    /// Scroll only as far as needed, on both axes: no movement when the
    /// element is already visible. The default.
    #[default]
    Nearest,
    /// Centre the element in the viewport, on both axes.
    Center,
}

impl ScrollMode {
    /// The `scrollIntoView` options for this mode, or `None` for no scroll.
    /// `behavior: 'instant'` so a position read straight after is not
    /// mid-way through a smooth scroll.
    fn js_options(self) -> Option<&'static str> {
        match self {
            ScrollMode::None => None,
            ScrollMode::Nearest => Some("{block:'nearest',inline:'nearest',behavior:'instant'}"),
            ScrollMode::Center => Some("{block:'center',inline:'center',behavior:'instant'}"),
        }
    }
}

/// Options for `act` beyond what it acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActOpts {
    pub scroll: ScrollMode,
    /// Wait for the page to settle after the action (`wait_after: "settle"`)
    /// and report what happened: `navigated`, `requests_started`, `settled`.
    pub settle: bool,
    /// Bound for the settle wait.
    pub timeout_ms: u64,
}

/// The `timeout_ms` default of a settle wait.
pub const ACT_SETTLE_TIMEOUT_MS: u64 = 10_000;

impl Default for ActOpts {
    fn default() -> Self {
        ActOpts {
            scroll: ScrollMode::default(),
            settle: false,
            timeout_ms: ACT_SETTLE_TIMEOUT_MS,
        }
    }
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
    /// [`BrowserBackend::act_masked`] with [`ActOpts`]: how the element is
    /// scrolled to, and whether to wait for the page to settle afterwards.
    /// Backends that do neither ignore the options.
    async fn act_opts(
        &self,
        target: &str,
        locator: Locator<'_>,
        action: &str,
        value: Option<&str>,
        secret: bool,
        opts: ActOpts,
    ) -> Result<Value, BrowserError> {
        let _ = opts;
        self.act_masked(target, locator, action, value, secret)
            .await
    }
    /// Set files on an `<input type=file>` located like `act` does. `files`
    /// are already-resolved absolute paths (the module layer jails and checks
    /// them); the backend only drives the browser. Chrome only.
    async fn upload(
        &self,
        target: &str,
        locator: Locator<'_>,
        files: &[String],
    ) -> Result<Value, BrowserError> {
        let _ = (target, locator, files);
        Err(BrowserError::Unsupported(
            "browser_upload needs the CDP (Chrome) engine".into(),
        ))
    }
    /// Wait for a settle signal (`selector` / `navigation` / `network_idle`).
    async fn wait(
        &self,
        target: &str,
        cond: &str,
        arg: Option<&str>,
        timeout_ms: u64,
    ) -> Result<Value, BrowserError>;
    /// [`BrowserBackend::wait`] with `nav_window_ms`: for `navigation` after a
    /// click, submit or key press, how long to keep expecting a navigation
    /// that has not begun yet (a handler that defers `location`) before
    /// settling on the loaded page with `navigated: false`. `None` is the
    /// default (2 s); values above 30 s are capped. Other conditions ignore it.
    async fn wait_window(
        &self,
        target: &str,
        cond: &str,
        arg: Option<&str>,
        timeout_ms: u64,
        nav_window_ms: Option<u64>,
    ) -> Result<Value, BrowserError> {
        let _ = nav_window_ms;
        self.wait(target, cond, arg, timeout_ms).await
    }
    /// Screenshot the page or one element.
    async fn screenshot(&self, target: &str, node_ref: Option<&str>) -> Result<Shot, BrowserError>;
    /// Start recording a tab to numbered JPEG frames under `media_dir`.
    async fn screencast_start(
        &self,
        target: &str,
        media_dir: &std::path::Path,
        opts: crate::screencast::ScreencastOpts,
    ) -> Result<Value, BrowserError> {
        let _ = (target, media_dir, opts);
        Err(BrowserError::Unsupported(
            "this backend cannot record video".into(),
        ))
    }
    /// Stop a recording (by tab or recording id) and encode it.
    async fn screencast_stop(
        &self,
        target: Option<&str>,
        recording_id: Option<&str>,
        keep_frames: bool,
    ) -> Result<Value, BrowserError> {
        let _ = (target, recording_id, keep_frames);
        Err(BrowserError::Unsupported(
            "this backend cannot record video".into(),
        ))
    }
    /// The active recordings.
    async fn screencast_status(&self) -> Result<Value, BrowserError> {
        Ok(json!({ "recordings": [] }))
    }
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
    /// [`BrowserBackend::eval`] with a time limit and an optional detached
    /// mode (see [`EvalOptions`]). A backend that cannot honour an option says
    /// so rather than ignoring it; the default only supports plain `eval`.
    async fn eval_with(
        &self,
        target: &str,
        expression: &str,
        opts: &EvalOptions,
    ) -> Result<Value, BrowserError> {
        if opts.detached {
            return Err(BrowserError::Unsupported(
                "detached eval needs the CDP (Chrome) engine".into(),
            ));
        }
        self.eval(target, expression).await
    }
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
    /// Where the showcase pointer last was, per tab, in viewport CSS px. A
    /// navigation resets the in-page cursor; this lets the next glide start
    /// where the viewer last saw it instead of off-screen.
    cursor_pos: Mutex<HashMap<String, (f64, f64)>>,
    /// Per target: the document-identity marker planted on the document an
    /// action (goto, reload, click, submit, press) was about to leave. A
    /// `wait navigation` that finds one waits for a document without it.
    nav_pending: Mutex<HashMap<String, NavPending>>,
    /// Tabs being watched across navigations (the recorder), by target id.
    observers: Mutex<HashMap<String, Observer>>,
    /// Video recordings (`browser_screencast`), each on its own session.
    screencasts: crate::screencast::ScreencastHub,
}

/// Longest semantic intent kept in a snapshot node. Mirrors `INTENT_MAX` in
/// `mcp-a11y/src/flatten.rs`.
const INTENT_MAX: usize = 48;

/// Hard cap on one node's serialized `bound_state`. The injected serializer
/// already bounds depth, width and string length; this is the backstop that
/// also covers the Safari path and anything the script missed. A larger value
/// is replaced by `{"truncated":true,"bytes":N}`.
const BOUND_STATE_MAX_BYTES: usize = 2048;
/// Deepest object nesting the injected serializer keeps.
const BOUND_STATE_JS_DEPTH: usize = 4;
/// Most keys per object and items per array the injected serializer keeps.
const BOUND_STATE_JS_KEYS: usize = 20;
/// Longest string (and key) the injected serializer keeps, in UTF-16 units.
const BOUND_STATE_JS_STRING: usize = 200;
/// Most objects the injected serializer will visit for one node, so a wide
/// but shallow structure cannot stall the page.
const BOUND_STATE_JS_BUDGET: usize = 200;

/// A semantic intent as a bare identifier (`add_to_cart`), or nothing. It comes
/// from page attributes (`data-intent`, ids, canvas regions), so anything
/// outside `[A-Za-z0-9_.-]` is dropped rather than escaped: an intent is a
/// label, not text to quote. Same rule as `intent_token` in
/// `mcp-a11y/src/flatten.rs`; duplicated because this crate does not depend
/// on `mcp-a11y` and the rule is five lines.
fn intent_token(raw: &str) -> Option<String> {
    let t: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        .take(INTENT_MAX)
        .collect();
    (!t.is_empty()).then_some(t)
}

/// `bound_state` unchanged when its JSON fits [`BOUND_STATE_MAX_BYTES`], else a
/// marker saying how large it was.
fn cap_bound_state(v: Value) -> Value {
    if v.is_null() {
        return v;
    }
    let bytes = serde_json::to_vec(&v).map_or(usize::MAX, |b| b.len());
    if bytes <= BOUND_STATE_MAX_BYTES {
        v
    } else {
        json!({ "truncated": true, "bytes": bytes })
    }
}

/// Apply the intent and `bound_state` rules to every node of a snapshot. Both
/// fields are page-controlled, so this runs on every path that returns page
/// script output (Chrome and Safari alike).
fn sanitize_snapshot_semantics(snap: &mut Value) {
    let Some(nodes) = snap.get_mut("nodes").and_then(Value::as_array_mut) else {
        return;
    };
    for node in nodes {
        let Some(obj) = node.as_object_mut() else {
            continue;
        };
        if let Some(i) = obj.get_mut("semantic_intent") {
            *i = i
                .as_str()
                .and_then(intent_token)
                .map_or(Value::Null, Value::String);
        }
        if let Some(b) = obj.get_mut("bound_state") {
            *b = cap_bound_state(b.take());
        }
    }
}

fn finish_snapshot(mut v: Value) -> Value {
    sanitize_snapshot_semantics(&mut v);
    v
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
/// `navigated: false`. Not applied to goto/reload/back/forward, which always
/// navigate. This is the default; `browser_wait` can set it per call
/// (`navigation_timeout_ms`) up to [`NAV_EXPECT_MAX_MS`].
const NAV_EXPECT_MS: u64 = 2_000;

/// The most a caller may raise the window to.
pub const NAV_EXPECT_MAX_MS: u64 = 30_000;

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

/// The showcase overlay script, made safe to splice into an action's script.
/// The overlay is decoration and runs in the page's own world, which can make
/// it throw (Trusted Types forbid its `innerHTML`, a page can lack a `head`).
/// Spliced bare, that throw rejects the action's whole async function: the
/// action's real error is replaced by the overlay's, and an action that would
/// have succeeded is reported as failed. Contained here, it only costs the
/// animation (the calls that use it are guarded the same way).
fn showcase_guarded(engine: &str) -> String {
    format!("try {{ {engine} }} catch (e) {{}}")
}

/// `return <expr>;` for `execute/sync`. The expression is trimmed first: a
/// `return` followed by a newline returns `undefined` (automatic semicolon
/// insertion) and never runs the expression, which a constant that begins with
/// a line break (`JS_CAPTURE_HOOK`) did, reporting success without arming.
fn safari_return(expr: &str) -> String {
    format!("return {};", expr.trim())
}

/// Script for `execute/async` that evaluates `expr` the way CDP
/// `Runtime.evaluate` does: as a script whose completion value is the result
/// (so `a(); b` and a plain expression both work), awaiting a returned
/// promise. Reports `{ok, value}` or `{ok:false, error}`; never throws.
///
/// `eval` is probed with a constant before the expression runs: when the probe
/// throws (a page CSP without `unsafe-eval`, Trusted Types, or a page that broke
/// `eval`), the reply is `{refused: true}` and nothing of `expr` has run, so the
/// caller can take the no-`eval` route without running it twice. A refusal is
/// never inferred from the expression's own error, which page code it calls can
/// word however it likes.
fn safari_eval_script(expr: &str) -> String {
    let src = serde_json::to_string(expr).unwrap_or_else(|_| "\"\"".into());
    format!(
        "var done = arguments[arguments.length - 1];\n\
         try {{ (0, eval)('0'); }} catch(e) {{ done({{ok:false,refused:true,error:String(e)}}); return; }}\n\
         try {{\n\
           Promise.resolve((0, eval)({src})).then(\n\
             function(v){{ done({{ok:true,value:v===undefined?null:v}}); }},\n\
             function(e){{ done({{ok:false,error:String(e)}}); }});\n\
         }} catch(e) {{ done({{ok:false,error:String(e)}}); }}"
    )
}

/// `execute/async` script that runs `code` as the WebDriver script body, with
/// no `eval`: the driver injects that body itself, so a page CSP without
/// `unsafe-eval` does not block it. `expression` wraps the code in `( ... )`
/// (its value is the result); otherwise it is a function body, whose `return`
/// value is the result. A returned promise is awaited either way. The newline
/// before the closing token keeps a trailing `//` comment from eating it.
fn safari_noeval_script(code: &str, expression: bool) -> String {
    let value = if expression {
        format!("({code}\n)")
    } else {
        format!("(function(){{\n{code}\n}}).call(window)")
    };
    format!(
        "var done = arguments[arguments.length - 1];\n\
         try {{\n\
           Promise.resolve({value}).then(\n\
             function(v){{ done({{ok:true,value:v===undefined?null:v}}); }},\n\
             function(e){{ done({{ok:false,error:String(e)}}); }});\n\
         }} catch(e) {{ done({{ok:false,error:String(e)}}); }}"
    )
}

/// Refuse a Safari (WebKit) target for a feature built on CDP, saying so,
/// rather than letting it fail later as a target that "was not found".
fn require_cdp_target(target: &str, what: &str) -> Result<(), BrowserError> {
    if target.starts_with("safari-") {
        return Err(BrowserError::Unsupported(format!(
            "{what} needs the CDP (Chrome) engine; the WebKit engine cannot do it"
        )));
    }
    Ok(())
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
    expect_ms: u64,
) -> Option<bool> {
    if ready_state != "complete" {
        return None;
    }
    if marker != Some(pending_token) {
        return Some(true);
    }
    if !certain && since_set >= std::time::Duration::from_millis(expect_ms) {
        return Some(false);
    }
    None
}

/// [`nav_probe_verdict`] for an action that is being settled: also gives up
/// waiting for a navigation as soon as the loaded, unreplaced document has
/// started a fetch / XHR since the action. That is the page working in place
/// (an htmx swap, an API call), and waiting out the whole navigation window
/// for it would make every such click cost seconds.
fn settle_nav_verdict(
    marker: Option<&str>,
    ready_state: &str,
    pending_token: &str,
    requests_since: u64,
    since_set: std::time::Duration,
    expect_ms: u64,
) -> Option<bool> {
    match nav_probe_verdict(
        marker,
        ready_state,
        pending_token,
        false,
        since_set,
        expect_ms,
    ) {
        None if ready_state == "complete"
            && marker == Some(pending_token)
            && requests_since > 0 =>
        {
            Some(false)
        }
        v => v,
    }
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
            cursor_pos: Mutex::new(HashMap::new()),
            nav_pending: Mutex::new(HashMap::new()),
            observers: Mutex::new(HashMap::new()),
            screencasts: crate::screencast::ScreencastHub::default(),
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
        expect_ms: u64,
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
                    expect_ms,
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

    /// What `browser_act wait_after:"settle"` does once the action has run:
    /// wait for a navigation it started to land, then for htmx (when the page
    /// has it), then for the network to go quiet, all bounded by `timeout_ms`.
    /// Never fails: the action happened, so a wait that runs out is reported
    /// as `settled: false` with `settle_error`.
    async fn settle_after_act(
        &self,
        target: &str,
        nav_token: Option<String>,
        timeout_ms: u64,
    ) -> Value {
        use tokio::time::{sleep, Duration, Instant};
        let started = Instant::now();
        let deadline = started + Duration::from_millis(timeout_ms.clamp(50, 60_000));
        let left = || {
            deadline
                .saturating_duration_since(Instant::now())
                .as_millis() as u64
        };
        let mut navigated = false;
        let mut error: Option<String> = None;
        let mut htmx = false;
        let mut c = match self.conn(target).await {
            Ok(c) => c,
            Err(e) => {
                return json!({
                    "navigated": false, "requests_started": 0, "settled": false,
                    "settle_error": berr_msg(&e),
                })
            }
        };
        if let Some(token) = nav_token {
            // A fetch / XHR / htmx request that began on the document the
            // click ran on is the page doing its work in place; stop waiting
            // for a navigation then (see `settle_nav_verdict`).
            loop {
                if let Ok(p) = Self::eval_value(&mut c, JS_ACT_PROBE).await {
                    htmx = p.get("htmx").and_then(Value::as_bool) == Some(true);
                    let verdict = settle_nav_verdict(
                        p.get("tok").and_then(Value::as_str),
                        p.get("state").and_then(Value::as_str).unwrap_or(""),
                        &token,
                        p.get("req").and_then(Value::as_u64).unwrap_or(0),
                        started.elapsed(),
                        NAV_EXPECT_MS,
                    );
                    if let Some(nav) = verdict {
                        navigated = nav;
                        self.clear_nav_pending(target, Some(&token));
                        break;
                    }
                }
                if Instant::now() >= deadline {
                    error = Some(format!("the page was still loading after {timeout_ms}ms"));
                    break;
                }
                sleep(Duration::from_millis(50)).await;
            }
        }
        if error.is_none() {
            if let Ok(p) = Self::eval_value(&mut c, JS_ACT_PROBE).await {
                htmx = p.get("htmx").and_then(Value::as_bool) == Some(true);
            }
            if htmx {
                if let Err(e) = self
                    .wait_window(target, "htmx_settled", None, left(), None)
                    .await
                {
                    error = Some(berr_msg(&e));
                }
            }
        }
        if error.is_none() {
            if let Err(e) = self
                .wait_window(target, "network_idle", None, left(), None)
                .await
            {
                error = Some(berr_msg(&e));
            }
        }
        let requests = match Self::eval_value(&mut c, JS_ACT_PROBE).await {
            Ok(p) => p.get("req").and_then(Value::as_u64).unwrap_or(0),
            Err(_) => 0,
        };
        let mut out = json!({
            "navigated": navigated,
            "requests_started": requests,
            "settled": error.is_none(),
        });
        if let (Some(e), Some(m)) = (error, out.as_object_mut()) {
            m.insert("settle_error".into(), json!(e));
        }
        out
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
                            .execute_sync(&safari_return(script), &[])
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

    /// Move the real pointer from `from` to `to` in eased `mouseMoved` steps
    /// spread over `glide_ms` (a single event when it is 0). These are trusted
    /// events: the page sees `mousemove`/`mouseover`, and CSS `:hover` applies.
    /// Says whether the last one, at `to`, was delivered; a failure part-way
    /// is not an error of the action (the pointer is decoration).
    async fn glide_mouse(c: &mut CdpConn, from: (f64, f64), to: (f64, f64), glide_ms: u64) -> bool {
        let path = crate::showcase::glide_path(from, to, glide_ms);
        let delay = std::time::Duration::from_millis(glide_ms / path.len().max(1) as u64);
        let last = path.len().saturating_sub(1);
        for (i, (x, y)) in path.into_iter().enumerate() {
            let sent = c
                .call(
                    "Input.dispatchMouseEvent",
                    json!({ "type": "mouseMoved", "x": x, "y": y, "button": "none", "buttons": 0 }),
                )
                .await;
            if i == last {
                return sent.is_ok();
            }
            if sent.is_err() {
                return false;
            }
            tokio::time::sleep(delay).await;
        }
        false
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

    /// Bring the browser's active page (the first in `/json/list`, which Chrome
    /// keeps in most-recently-active order) to the front. Best effort; says
    /// whether it worked. The focus emulation set here ends with this
    /// connection (see [`Self::emulate_focus`]); what keeps a headed
    /// browser's timers running afterwards is the launch flags.
    async fn foreground_active_page(&self, browser_id: u32) -> bool {
        let Ok(list) = self.tabs(browser_id, "list", None, None).await else {
            return false;
        };
        let Some(target) = list
            .get("tabs")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|t| t.get("target_id"))
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            return false;
        };
        let Ok(mut c) = self.conn(&target).await else {
            return false;
        };
        Self::emulate_focus(&mut c).await;
        c.call("Page.bringToFront", json!({})).await.is_ok()
    }

    /// Make the page believe it has focus (`document.hasFocus()`, focus and
    /// blur events) even when its window is behind another. Best effort. The
    /// emulation belongs to the DevTools session that asked for it and ends
    /// with it, so it cannot be set once and left.
    async fn emulate_focus(c: &mut CdpConn) {
        c.call(
            "Emulation.setFocusEmulationEnabled",
            json!({ "enabled": true }),
        )
        .await
        .ok();
    }

    /// Run JS in the page and return the deserialized value (or a JS-exception
    /// error). Enables the Runtime domain first.
    async fn eval_value(c: &mut CdpConn, expr: &str) -> Result<Value, BrowserError> {
        Self::eval_value_in(c, expr, None).await
    }

    /// The execution context of the recorder's isolated world in the tab's
    /// top document, when a recording is running on `target`. The act scripts
    /// run there so the events they dispatch can be told apart from the
    /// page's own (see `JS_ARM`). `None` when nothing is recording, or the
    /// world cannot be found (the act then runs in the page's world and the
    /// recorder drops its synthetic events).
    async fn recorder_context(&self, target: &str, c: &mut CdpConn) -> Option<i64> {
        let live = self
            .observers
            .lock()
            .ok()?
            .get(target)
            .is_some_and(|o| !o.task.is_finished());
        if !live {
            return None;
        }
        let tree = c.call("Page.getFrameTree", json!({})).await.ok()?;
        let top = tree
            .get("frameTree")?
            .get("frame")?
            .get("id")?
            .as_str()?
            .to_string();
        // Enabling Runtime announces every context that exists now.
        c.keep_events(true);
        let enabled = c.call("Runtime.enable", json!({})).await;
        c.keep_events(false);
        let events = c.take_events();
        enabled.ok()?;
        events.iter().rev().find_map(|ev| {
            if ev.get("method").and_then(Value::as_str) != Some("Runtime.executionContextCreated") {
                return None;
            }
            let ctx = ev.get("params")?.get("context")?;
            let aux = ctx.get("auxData")?;
            (ctx.get("name").and_then(Value::as_str) == Some(RECORDER_WORLD)
                && aux.get("frameId").and_then(Value::as_str) == Some(top.as_str())
                && aux.get("isDefault").and_then(Value::as_bool) == Some(false))
            .then(|| ctx.get("id").and_then(Value::as_i64))
            .flatten()
        })
    }

    /// `eval_value` in a given execution context (the page's own when `None`).
    async fn eval_value_in(
        c: &mut CdpConn,
        expr: &str,
        context: Option<i64>,
    ) -> Result<Value, BrowserError> {
        c.call("Runtime.enable", json!({})).await.ok();
        let mut params = json!({
            "expression": expr,
            "returnByValue": true,
            "awaitPromise": true,
            "userGesture": true
        });
        if let (Some(id), Some(m)) = (context, params.as_object_mut()) {
            m.insert("contextId".into(), json!(id));
        }
        let r = c.call("Runtime.evaluate", params).await?;
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

/// JS expression resolving a [`Locator`] to an element (needs `JS_XPATH` and
/// `JS_FIND` in scope): a `ref` via XPath, or a selector via `__find`.
fn locator_js(locator: Locator<'_>) -> String {
    match locator {
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
    }
}

/// JS that resolves `__RESOLVE__` and settles on the `<input type=file>` it
/// stands for, or throws a string saying what was found instead. A `<label>`
/// goes to its `control`; anything else that is not a file input gets one try
/// at a single `input[type=file]` descendant. More than one file needs
/// `multiple`.
const JS_UPLOAD_INPUT: &str = r#"(function(){
  {JS_XPATH}
  {JS_FIND}
  var n = __COUNT__;
  var el;
  try { el = __RESOLVE__; } catch(e) { throw String(e && e.message ? e.message : e); }
  if(!el) throw 'element not found';
  if(el.__is_canvas_target) throw 'a canvas region is not a file input';
  function isFile(x){ return !!x && x.tagName === 'INPUT' && String(x.type).toLowerCase() === 'file'; }
  var found = el;
  if(el.tagName === 'LABEL' && el.control) el = el.control;
  if(!isFile(el) && el.querySelectorAll){
    var inner = el.querySelectorAll('input[type=file]');
    if(inner.length === 1) el = inner[0];
  }
  if(!isFile(el)){
    var d = found.tagName ? found.tagName.toLowerCase() : String(found.nodeName);
    if(found.tagName === 'INPUT') d += '[type=' + found.type + ']';
    throw 'the element is a <' + d + '>, not a file input; target the input[type=file] itself (it is often hidden), e.g. query "input[type=file]"';
  }
  if(el.disabled) throw 'the file input is disabled';
  if(n > 1 && !el.multiple) throw 'the file input has no multiple attribute but ' + n + ' files were given; give one path';
  return el;
})()"#;

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
        "ArrowDown" => Some(KeySpec {
            key: "ArrowDown",
            code: "ArrowDown",
            vk: 40,
            text: None,
        }),
        "ArrowUp" => Some(KeySpec {
            key: "ArrowUp",
            code: "ArrowUp",
            vk: 38,
            text: None,
        }),
        "ArrowLeft" => Some(KeySpec {
            key: "ArrowLeft",
            code: "ArrowLeft",
            vk: 37,
            text: None,
        }),
        "ArrowRight" => Some(KeySpec {
            key: "ArrowRight",
            code: "ArrowRight",
            vk: 39,
            text: None,
        }),
        "Home" => Some(KeySpec {
            key: "Home",
            code: "Home",
            vk: 36,
            text: None,
        }),
        "End" => Some(KeySpec {
            key: "End",
            code: "End",
            vk: 35,
            text: None,
        }),
        "PageUp" => Some(KeySpec {
            key: "PageUp",
            code: "PageUp",
            vk: 33,
            text: None,
        }),
        "PageDown" => Some(KeySpec {
            key: "PageDown",
            code: "PageDown",
            vk: 34,
            text: None,
        }),
        "Backspace" => Some(KeySpec {
            key: "Backspace",
            code: "Backspace",
            vk: 8,
            text: None,
        }),
        "Delete" => Some(KeySpec {
            key: "Delete",
            code: "Delete",
            vk: 46,
            text: None,
        }),
        // The space bar types a character, so unlike the others it carries text.
        "Space" | " " => Some(KeySpec {
            key: " ",
            code: "Space",
            vk: 32,
            text: Some(" "),
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
    ///
    /// With `bypass_cache` the load (the document and everything it pulls in)
    /// goes to the network: Chrome's HTTP cache is switched off for this
    /// session, which ends when the call returns, so the setting never leaks
    /// into the page's later loads. A server that is down is then an error
    /// rather than a page served from disk.
    async fn goto_and_wait(
        &self,
        target: &str,
        url: &str,
        timeout_ms: u64,
        bypass_cache: bool,
    ) -> Result<(), BrowserError> {
        if let Err(denied) = self.nav.check(url).await {
            return Err(BrowserError::PermissionDenied(denied.message()));
        }
        let mut c = self.conn(target).await?;
        if bypass_cache {
            c.call("Network.enable", json!({})).await?;
            c.call("Network.setCacheDisabled", json!({ "cacheDisabled": true }))
                .await?;
        }
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
///
/// `__find_all` returns the whole ranked list; `__find` picks one and leaves the
/// size of the list in `__find.count`, which `browser_act` reports as `matches`.
/// `by: text` is ranked (see `__text_matches`), so the first match is the one
/// a person would mean, not the first element in the page that mentions it.
const JS_FIND: &str = r#"
function __norm(s){ return String(s == null ? '' : s).replace(/\s+/g, ' ').trim().toLowerCase(); }
// The strings an element answers to: its text, a button input's value, its aria-label.
function __texts(el){
  var t = el.innerText;
  if(t == null || t === '') t = el.textContent;
  var out = [__norm(t)];
  if(el.tagName === 'INPUT'){
    var ty = String(el.type).toLowerCase();
    if(ty === 'button' || ty === 'submit' || ty === 'reset') out.push(__norm(el.value));
  }
  var al = el.getAttribute && el.getAttribute('aria-label');
  if(al) out.push(__norm(al));
  return out;
}
var __ACTIONABLE = 'button, a[href], input:not([type=hidden]), select, textarea, summary, label, [role=button], [role=link], [role=menuitem], [role=option], [role=tab], [role=checkbox], [role=radio], [role=switch], [role=treeitem], [onclick], [contenteditable=true]';
// Text matches, best first: exact before substring (and when any is exact the
// substring ones are dropped), clickable before not, then document order. A
// candidate is the innermost element whose text holds the query, lifted to the
// control around it, so `<button><span>Next</span></button>` is the button and
// the sentence `Click on "next"` only ranks as a last resort.
function __text_matches(root, q){
  q = __norm(q);
  if(!q || !root.querySelectorAll) return [];
  var all = root.querySelectorAll('*'), n = all.length, has = new Array(n), tx = new Array(n), skip = /^(SCRIPT|STYLE|NOSCRIPT|TEMPLATE|HEAD|TITLE|META|LINK)$/;
  var pos = new Map();
  for(var i=0; i<n; i++){
    pos.set(all[i], i);
    tx[i] = skip.test(all[i].tagName) ? [] : __texts(all[i]);
    has[i] = tx[i].some(function(t){ return t.indexOf(q) >= 0; });
  }
  var best = new Map();
  for(var j=0; j<n; j++){
    if(!has[j]) continue;
    var kids = all[j].children, inner = true;
    for(var k=0; k<kids.length; k++){ if(has[pos.get(kids[k])]){ inner = false; break; } }
    if(!inner) continue;
    var el = all[j], lifted = el.closest ? el.closest(__ACTIONABLE) : null;
    var act = !!(lifted && (lifted === root || root.contains(lifted)));
    if(!act) lifted = el;
    var exact = tx[j].indexOf(q) >= 0;
    if(!exact && act){
      var li = pos.get(lifted);
      exact = li !== undefined && tx[li].indexOf(q) >= 0;
    }
    var prev = best.get(lifted);
    if(!prev) best.set(lifted, {el: lifted, exact: exact, act: act, ord: j});
    else if(exact) prev.exact = true;
  }
  var list = Array.from(best.values());
  if(list.some(function(m){ return m.exact; })) list = list.filter(function(m){ return m.exact; });
  list.sort(function(a, b){ return (b.act - a.act) || (a.ord - b.ord); });
  return list.map(function(m){ return m.el; });
}
// What `browser_act` reports it acted on: tag and a short name. A field is named
// by its label, never its value (a value can be a secret); a button-type input's
// value is its caption, so that one is used.
function __label_text(n){
  if(n.nodeType === 3) return n.nodeValue;
  if(n.nodeType !== 1 || /^(INPUT|SELECT|TEXTAREA|SCRIPT|STYLE)$/.test(n.tagName)) return '';
  var s = '';
  for(var c = n.firstChild; c; c = c.nextSibling) s += __label_text(c) + ' ';
  return s;
}
function __target(el){
  var tag = (el.tagName || '').toLowerCase(), t = '';
  var ty = tag === 'input' ? String(el.type).toLowerCase() : '';
  var btn = ty === 'button' || ty === 'submit' || ty === 'reset';
  if((tag === 'input' || tag === 'select' || tag === 'textarea' || el.isContentEditable) && !btn){
    var at = function(a){ return el.getAttribute ? el.getAttribute(a) : null; };
    t = at('aria-label');
    if(!t && el.labels && el.labels.length) t = __label_text(el.labels[0]);
    t = t || at('placeholder') || at('name') || '';
  } else if(btn){
    t = el.value || el.getAttribute('aria-label') || '';
  } else {
    t = el.innerText || el.textContent || (el.getAttribute && el.getAttribute('aria-label')) || '';
  }
  return {tag: tag, text: String(t).replace(/\s+/g, ' ').trim().slice(0, 80)};
}
function __find(by, q, within, textFilter, index){
  var matches = __find_all(by, q, within, textFilter);
  __find.count = matches.length;
  var idx = (typeof index === 'number' && index >= 0) ? index : 0;
  return matches[idx] || null;
}
function __find_all(by, q, within, textFilter){
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
    matches = __text_matches(root, q);
  }

  if(textFilter && typeof textFilter === 'string' && textFilter.length > 0) {
    var tf = textFilter.toLowerCase();
    matches = matches.filter(function(el){
      var t = (el.innerText || el.textContent || el.value || '').trim().toLowerCase();
      return t.indexOf(tf) >= 0;
    });
  }
  return matches;
}
"#;

/// Idempotent htmx listeners, as a JS function `__hx_hook()` returning the
/// shared state (or `null` while `window.htmx` is absent).
///
/// `htmx` has no "is anything in flight" API, so the hook installs
/// capture-phase listeners on `document` for `htmx:beforeRequest` /
/// `htmx:afterRequest` / `htmx:afterSettle`, keeping an in-flight counter, the
/// number of requests seen (`seen`) and the time of the last event. The act
/// path installs it *before* the action, so a request the action starts is
/// counted even when it begins late (a debounced or delayed trigger).
const JS_HTMX_HOOK: &str = r#"
function __hx_hook(){
  if (!window.htmx) return null;
  var st = window.__agentctl_htmx;
  if (!st) {
    st = window.__agentctl_htmx = { inflight: 0, last: Date.now(), seen: 0, mark: 0 };
    var touch = function(){ st.last = Date.now(); };
    document.addEventListener('htmx:beforeRequest', function(){ st.inflight++; st.seen++; touch(); }, true);
    document.addEventListener('htmx:afterRequest', function(){ if (st.inflight > 0) st.inflight--; touch(); }, true);
    document.addEventListener('htmx:afterSettle', touch, true);
  }
  return st;
}"#;

/// Idempotent fetch / XHR counter, as a JS function `__net_hook()` returning
/// the shared state `{inflight, started, last, mark, act_at}`. `started` counts
/// every fetch and XHR (htmx uses XHR) begun since the hook went in; the act
/// path sets `mark` and `act_at`, so "started since the last act" is
/// `started - mark`. A request already in flight when the hook goes in is not
/// seen until it completes (the Performance API lists it only then), which is
/// why the act path installs the hook before it acts.
const JS_NET_HOOK: &str = r#"
function __net_hook(){
  var st = window.__agentctl_net;
  if (st) return st;
  st = window.__agentctl_net = { inflight: 0, started: 0, last: Date.now(), mark: 0, act_at: null };
  var begin = function(){ st.inflight++; st.started++; st.last = Date.now(); };
  var end = function(){ if (st.inflight > 0) st.inflight--; st.last = Date.now(); };
  try {
    if (window.fetch) {
      var of = window.fetch;
      window.fetch = function(){
        begin();
        var p;
        try { p = of.apply(this, arguments); } catch(e) { end(); throw e; }
        p.then(end, end);
        return p;
      };
    }
    var os = XMLHttpRequest.prototype.send;
    XMLHttpRequest.prototype.send = function(){
      var x = this, done = false;
      begin();
      x.addEventListener('loadend', function(){ if (!done) { done = true; end(); } });
      try { return os.apply(this, arguments); } catch(e) { if (!done) { done = true; end(); } throw e; }
    };
  } catch(e) {}
  return st;
}"#;

/// How long `htmx_settled` waits, after an act, for an htmx request to start
/// before taking it that none will (a debounced `hx-trigger` can begin
/// hundreds of milliseconds after the event).
const HTMX_GRACE_MS: u64 = 1_500;

/// Quiet period `network_idle` requires: no fetch/XHR in flight, and none
/// begun or finished for this long.
const NET_QUIET_MS: u64 = 500;

/// Run right before an action: install both hooks, then record "an action
/// happened now, with this many requests seen so far".
fn act_arm_js() -> String {
    format!(
        r#"(function(){{
  {JS_NET_HOOK}
  {JS_HTMX_HOOK}
  var n = __net_hook();
  n.mark = n.started; n.act_at = Date.now();
  var h = __hx_hook();
  if (h) h.mark = h.seen;
  return true;
}})()"#
    )
}

/// Probe: has HTMX finished all in-flight requests and DOM swaps?
///
/// Settled means: counter 0, no element carrying `htmx-request` /
/// `htmx-settling` / `htmx-swapping` (this also covers requests that started
/// before the hook was installed), no htmx event for a short quiet window, and
/// (right after an act) either a request was seen since it or the
/// [`HTMX_GRACE_MS`] grace has run out. Throws when `window.htmx` is absent so
/// a page without htmx is an error, not "settled".
fn htmx_settled_js() -> String {
    format!(
        r#"(function(){{
  {JS_HTMX_HOOK}
  var st = __hx_hook();
  if (!st) throw new Error('htmx not present on page');
  if (st.inflight > 0) return false;
  if (document.querySelector('.htmx-request, .htmx-settling, .htmx-swapping') !== null) return false;
  if (Date.now() - st.last < 100) return false;
  var n = window.__agentctl_net;
  if (n && n.act_at !== null && st.seen === st.mark && Date.now() - n.act_at < {HTMX_GRACE_MS}) return false;
  return document.readyState === 'complete' || document.readyState === 'interactive';
}})()"#
    )
}

/// Probe for `network_idle`: loaded, no fetch/XHR in flight, and nothing begun
/// or finished for [`NET_QUIET_MS`] (the Performance API's resource entries
/// cover requests the hook did not see begin).
fn net_idle_js() -> String {
    format!(
        r#"(function(){{
  {JS_NET_HOOK}
  var st = __net_hook();
  if (document.readyState !== 'complete') return false;
  if (st.inflight > 0) return false;
  if (Date.now() - st.last < {NET_QUIET_MS}) return false;
  try {{
    var es = performance.getEntriesByType('resource'), now = performance.now();
    for (var i = es.length - 1, k = 0; i >= 0 && k < 64; i--, k++) {{
      if (now - es[i].responseEnd < {NET_QUIET_MS}) return false;
    }}
  }} catch(e) {{}}
  return true;
}})()"#
    )
}

/// Page state for settling an act: the requests started since the act and
/// the navigation marker, in one read. Throws nothing; `req` is 0 when the
/// hook is absent (a replaced document).
const JS_ACT_PROBE: &str = r#"({
  tok: window.__agentctl_nav_token === undefined ? null : String(window.__agentctl_nav_token),
  state: document.readyState,
  req: window.__agentctl_net ? window.__agentctl_net.started - window.__agentctl_net.mark : 0,
  htmx: !!window.htmx
})"#;

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

/// Assign `value` / `checked` the way a user's input would, past any
/// instance-level override. React installs its own `value` accessor on each
/// controlled element to track the last value it saw, and ignores an `input`
/// event when the tracked value equals the new one; a plain `el.value = x`
/// goes through that accessor, so React concludes nothing changed. These walk
/// the prototype chain to the native setter (HTMLInputElement,
/// HTMLTextAreaElement, HTMLSelectElement) and call it on the element, which
/// leaves the tracker stale, so the event that follows is taken as a change.
const JS_SET_VALUE: &str = r#"function __nativeSet(el, prop, v){
    for(var o = Object.getPrototypeOf(el); o; o = Object.getPrototypeOf(o)){
      var d = Object.getOwnPropertyDescriptor(o, prop);
      if(d && d.set){ d.set.call(el, v); return; }
    }
    el[prop] = v;
  }
  function __setValue(el, v){ __nativeSet(el, 'value', v); }
  function __setChecked(el, v){ __nativeSet(el, 'checked', v); }"#;

/// What a `type` left in the field, for the result. A secret (an explicit
/// `secret`, a password or one-time-code field, a card field; the same test the
/// showcase typing HUD uses) is reported by length only. Also the test for
/// whether a real `Input.insertText` can go into an element: a plain text
/// input, a textarea or a contenteditable that is enabled and not read-only.
const JS_TYPE_HELPERS: &str = r#"function __readback(el, secret){
    var v = ('value' in el) ? el.value : el.textContent;
    v = v == null ? '' : String(v);
    var hide = !!secret;
    try {
      var t = String(el.getAttribute('type') || '').toLowerCase();
      var ac = String(el.getAttribute('autocomplete') || '').toLowerCase();
      if(t === 'password' || ac.indexOf('password') >= 0 || ac.indexOf('one-time-code') >= 0 || ac.indexOf('cc-') >= 0) hide = true;
    } catch(e) {}
    return hide ? {value_length: v.length} : {value_after: v};
  }
  function __canInsert(el){
    if(el.ownerDocument !== document || el.disabled || el.readOnly) return false;
    var tag = (el.tagName || '').toLowerCase();
    if(tag === 'textarea') return true;
    if(tag === 'input') return ['', 'text', 'search', 'url', 'tel', 'email', 'password', 'number'].indexOf(String(el.getAttribute('type') || '').toLowerCase()) >= 0;
    return !!el.isContentEditable;
  }
  function __selectContents(el){
    if('value' in el){ try { el.select(); return; } catch(e) {} }
    var r = document.createRange(); r.selectNodeContents(el);
    var sel = window.getSelection(); sel.removeAllRanges(); sel.addRange(r);
  }"#;

/// Lets the act scripts tell the recorder "the next `type` event on `el` is
/// mine". Only meaningful in the recorder's isolated world, where
/// `window.__agentctl_recorder` is the recorder's own; elsewhere (`__ISO__`
/// false) both helpers do nothing, so a page's same-named global is never
/// touched. One event is armed right before the one dispatch that uses it.
const JS_ARM: &str = r#"var __iso = __ISO__;
  function __arm(el, type){
    if(!__iso) return;
    var r = window.__agentctl_recorder;
    if(r && r.expect) r.expect.push({type: type, target: el});
  }
  function __disarm(){
    if(!__iso) return;
    var r = window.__agentctl_recorder;
    if(r && r.expect) r.expect.length = 0;
  }
  __disarm();"#;

/// In-page script that batches multiple form field updates and optional submit.
const JS_FILL_FORM: &str = r##"(async function(){
  {JS_XPATH}
  {JS_ARM}
  {JS_SET_VALUE}
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
  try {
  for(var i=0; i<fields.length; i++){
    var f = fields[i];
    var el = resolve(f);
    if(!el){
      errors.push({field: f.selector || f.ref || ('index_' + i), error: 'element not found'});
      continue;
    }
    try {
      if(el.scrollIntoView) el.scrollIntoView({block:'nearest', inline:'nearest', behavior:'instant'});
      if(el.focus) el.focus({preventScroll:true});
      var val = f.value;
      {JS_SHOWCASE_FIELD}
      var tag = (el.tagName || '').toLowerCase();
      var inputType = (el.getAttribute('type') || '').toLowerCase();
      var fType = (f.type || '').toLowerCase();
      if(tag === 'select' || fType === 'select'){
        __setValue(el, String(val == null ? '' : val));
        __arm(el, 'input');
        el.dispatchEvent(new Event('input', {bubbles: true}));
        __arm(el, 'change');
        el.dispatchEvent(new Event('change', {bubbles: true}));
        filled++;
      } else if(inputType === 'checkbox' || inputType === 'radio' || fType === 'checkbox' || fType === 'radio'){
        var shouldCheck = Boolean(val);
        if(el.checked !== shouldCheck){
          __setChecked(el, shouldCheck);
          __arm(el, 'input');
          el.dispatchEvent(new Event('input', {bubbles: true}));
          __arm(el, 'change');
          el.dispatchEvent(new Event('change', {bubbles: true}));
        }
        filled++;
      } else {
        if('value' in el){
          __setValue(el, (val == null ? '' : String(val)));
        } else {
          el.textContent = (val == null ? '' : String(val));
        }
        __arm(el, 'input');
        el.dispatchEvent(new Event('input', {bubbles: true}));
        __arm(el, 'change');
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
      __arm(subEl, 'click');
      if(subEl.click) subEl.click();
      else if(subEl.form && subEl.form.requestSubmit) subEl.form.requestSubmit();
      else if(subEl.form && subEl.form.submit) subEl.form.submit();
      submitted = true;
    } else if(submit.selector || submit.ref){
      errors.push({field: 'submit', error: 'submit element not found'});
    }
  }
  } finally { __disarm(); }
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
                for key in ["args", "background_throttling"] {
                    if spec.get(key).is_some() {
                        return Err(BrowserError::Unsupported(format!(
                            "launch.{key} is a Chromium option; Safari is started by safaridriver and takes no command-line flags"
                        )));
                    }
                }
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
        let owned_profile = started
            .as_ref()
            .and_then(|(_, d)| d.as_ref())
            .map(|d| d.to_string_lossy().into_owned());
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
        let mut out = json!({
            "browser_id": id,
            "host": host,
            "port": port,
            "browser": ver.get("Browser"),
            "protocol": ver.get("Protocol-Version"),
        });
        out["foregrounded"] = json!(self.foreground_active_page(id).await);
        // Only a profile we created (and will delete on disconnect) is reported.
        if let Some(dir) = owned_profile {
            out["owned_user_data_dir"] = json!(dir);
        }
        Ok(out)
    }

    fn shutdown(&self) {
        // Close the sessions that watch tabs; their new-document scripts go
        // with them.
        if let Ok(mut m) = self.observers.lock() {
            for (_, o) in m.drain() {
                o.task.abort();
            }
        }
        self.screencasts.abort_all();
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

        self.screencasts.abort_browser(browser_id);
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
                // Mark the document being left, like goto and reload do, so a
                // `wait navigation` that follows waits for the entry's page
                // instead of returning on this one while the load is slow.
                self.plant_nav_token(&mut c, target, true).await;
                c.keep_events(true);
                let went = c
                    .call(
                        "Page.navigateToHistoryEntry",
                        json!({ "entryId": entry_id }),
                    )
                    .await;
                c.keep_events(false);
                let early = c.take_events();
                if let Err(e) = went {
                    self.clear_nav_pending(target, None);
                    return Err(e);
                }
                // An entry made by `history.pushState` or a fragment change
                // keeps the document (and the marker): nothing to wait for.
                // Chrome reports it as soon as the history entry is applied.
                let within = |v: &Value| {
                    v.get("method").and_then(Value::as_str) == Some("Page.navigatedWithinDocument")
                };
                let same_document = early.iter().any(within)
                    || c.collect_events(&["Page.navigatedWithinDocument"], 150, 1)
                        .await
                        .is_ok_and(|e| !e.is_empty());
                if same_document {
                    self.clear_nav_pending(target, None);
                }
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

  // Page-controlled values (`data-intent`, `data-state`, React props, canvas
  // region fields) are bounded here so one hostile or huge page cannot stall
  // the snapshot; Rust re-checks everything (`sanitize_snapshot_semantics`).
  var SG_DEPTH = {BOUND_STATE_JS_DEPTH}, SG_KEYS = {BOUND_STATE_JS_KEYS}, SG_STR = {BOUND_STATE_JS_STRING}, SG_BUDGET = {BOUND_STATE_JS_BUDGET};
  function __sg_str(x){{
    return (typeof x === 'string') ? x.slice(0, SG_STR) : null;
  }}
  function __sg_safe(v){{
    var budget = SG_BUDGET, path = new WeakSet();
    function walk(x, d){{
      if(x === null || x === undefined) return null;
      var t = typeof x;
      if(t === 'string') return x.length > SG_STR ? x.slice(0, SG_STR) : x;
      if(t === 'number') return isFinite(x) ? x : null;
      if(t === 'boolean') return x;
      if(t !== 'object') return undefined; // function, symbol, bigint
      if(budget-- <= 0) return undefined;
      try {{
        if(x === window || (typeof Node !== 'undefined' && x instanceof Node)) return undefined;
        if(path.has(x)) return undefined; // cycle
        if(d >= SG_DEPTH) return undefined;
        path.add(x);
        var out;
        if(Array.isArray(x)){{
          out = [];
          for(var i = 0; i < x.length && i < SG_KEYS; i++){{
            var item = walk(x[i], d + 1);
            out.push(item === undefined ? null : item);
          }}
        }} else {{
          out = {{}};
          var keys = Object.keys(x), n = 0;
          for(var j = 0; j < keys.length && n < SG_KEYS; j++){{
            var val;
            try {{ val = walk(x[keys[j]], d + 1); }} catch(e){{ continue; }}
            if(val === undefined) continue;
            out[keys[j].slice(0, SG_STR)] = val;
            n++;
          }}
        }}
        path.delete(x);
        return out;
      }} catch(e){{ return undefined; }}
    }}
    try {{ var r = walk(v, 0); return r === undefined ? null : r; }} catch(e){{ return null; }}
  }}

  function deriveIntentRaw(el, role, tag, name){{
    if(!el) return null;
    var di = el.getAttribute ? (el.getAttribute('data-intent') || el.getAttribute('data-action') || el.getAttribute('data-testid')) : null;
    if(di) return di;
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

  function deriveIntent(el, role, tag, name){{
    try {{ return __sg_str(deriveIntentRaw(el, role, tag, name)); }} catch(e){{ return null; }}
  }}

  function extractBoundState(el){{
    try {{ return __sg_safe(extractBoundStateRaw(el)); }} catch(e){{ return null; }}
  }}

  function extractBoundStateRaw(el){{
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
          var regIntent = __sg_str(reg.intent) || __sg_str(reg.semantic_intent) || ('canvas_' + regName.toLowerCase().replace(/[^a-z0-9]+/g, '_'));
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
            bound_state: __sg_safe(reg.bound_state || reg.state || null),
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
                .execute_sync(&safari_return(&expr), &[])
                .await?;
            return Ok(finish_snapshot(v));
        }
        let mut c = c_opt.unwrap();
        Ok(finish_snapshot(Self::eval_value(&mut c, &expr).await?))
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
  {JS_FIND}
  var by={by_lit}, q={q}, all={all}, els=[];
  if(by==='css'){{ els=Array.from(document.querySelectorAll(q)); }}
  else if(by==='xpath'){{
    var r=document.evaluate(q,document,null,XPathResult.ORDERED_NODE_SNAPSHOT_TYPE,null);
    for(var i=0;i<r.snapshotLength;i++) els.push(r.snapshotItem(i));
  }} else {{ // text, ranked as browser_act's is
    els=__text_matches(document,q);
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
                .execute_sync(&safari_return(&expr), &[])
                .await?
        } else {
            let mut c = self.conn(target).await?;
            Self::eval_value(&mut c, &expr).await?
        };
        let count = v.as_array().map(|a| a.len()).unwrap_or(0);
        Ok(json!({ "matches": v, "count": count }))
    }

    async fn upload(
        &self,
        target: &str,
        locator: Locator<'_>,
        files: &[String],
    ) -> Result<Value, BrowserError> {
        if target.starts_with("safari-") {
            return Err(BrowserError::Unsupported(
                "browser_upload needs the CDP (Chrome) engine; the WebKit engine cannot set files on a file input".into(),
            ));
        }
        let mut c = self.conn(target).await?;
        c.call("Runtime.enable", json!({})).await.ok();
        let expr = JS_UPLOAD_INPUT
            .replace("{JS_XPATH}", JS_XPATH)
            .replace("{JS_FIND}", JS_FIND)
            .replace("__COUNT__", &files.len().to_string())
            .replace("__RESOLVE__", &locator_js(locator));
        // The element is kept as a remote object, not copied by value: that
        // handle is what `DOM.setFileInputFiles` takes. The page's own world
        // is used (nothing here dispatches synthetic events, so the
        // recorder's isolated world has no part in it).
        let r = c
            .call(
                "Runtime.evaluate",
                json!({ "expression": expr, "returnByValue": false, "userGesture": true }),
            )
            .await?;
        if let Some(exc) = r.get("exceptionDetails") {
            let text = exc
                .get("exception")
                .and_then(|e| e.get("value").or_else(|| e.get("description")))
                .and_then(Value::as_str)
                .or_else(|| exc.get("text").and_then(Value::as_str))
                .unwrap_or("javascript error");
            return Err(BrowserError::Failed(format!("upload: {text}")));
        }
        let object_id = r
            .get("result")
            .and_then(|o| o.get("objectId"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| BrowserError::Failed("upload: no element to set files on".into()))?;
        let multiple = c
            .call(
                "Runtime.callFunctionOn",
                json!({
                    "objectId": object_id,
                    "functionDeclaration": "function(){ return this.multiple === true; }",
                    "returnByValue": true
                }),
            )
            .await
            .ok()
            .and_then(|v| v.get("result")?.get("value")?.as_bool())
            .unwrap_or(false);
        // Chrome fires the trusted `input` and `change` events itself.
        let set = c
            .call(
                "DOM.setFileInputFiles",
                json!({ "files": files, "objectId": object_id }),
            )
            .await;
        c.call("Runtime.releaseObject", json!({ "objectId": object_id }))
            .await
            .ok();
        set?;
        let mut out = json!({ "ok": true, "input_multiple": multiple });
        self.note_dialogs(target, &mut c, &mut out);
        Ok(out)
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
        self.act_opts(target, locator, action, value, secret, ActOpts::default())
            .await
    }

    async fn act_opts(
        &self,
        target: &str,
        locator: Locator<'_>,
        action: &str,
        value: Option<&str>,
        secret: bool,
        opts: ActOpts,
    ) -> Result<Value, BrowserError> {
        let is_safari = target.starts_with("safari-");
        if is_safari && opts.settle {
            return Err(BrowserError::Unsupported(
                "wait_after 'settle' needs the CDP (Chrome) engine; use browser_wait on the WebKit engine".into(),
            ));
        }
        let press_key = if action == "press" {
            if is_safari {
                return Err(BrowserError::Unsupported(
                    "act 'press' is not supported on the Safari engine".into(),
                ));
            }
            Some(key_event_spec(value.unwrap_or("")).ok_or_else(|| {
                BrowserError::Failed(format!(
                    "act 'press' needs a supported key in 'value' (Enter, Escape, Tab, ArrowDown, ArrowUp, ArrowLeft, ArrowRight, Home, End, PageUp, PageDown, Backspace, Delete, Space), got '{}'",
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
        // Focus emulation lasts as long as this connection, and every tool
        // call opens its own: ask for it here so a headed window sitting
        // behind others still sees focus/blur and `:focus` during the act.
        if let Some(c) = c_opt.as_mut() {
            Self::emulate_focus(c).await;
        }
        // While this tab is being recorded, the script runs in the recorder's
        // isolated world: the DOM is the same, but there it can tell the
        // recorder which synthetic events are agentctl's own (`JS_ARM`).
        // A canvas region is published by the page as a property of the
        // canvas, which an isolated world cannot see; its click is a real CDP
        // pointer event anyway, so nothing needs arming.
        let canvas_ref = matches!(&locator, Locator::Ref(r) if r.contains("::canvas["));
        let iso = match c_opt.as_mut() {
            Some(c) if !canvas_ref => self.recorder_context(target, c).await,
            _ => None,
        };
        let js_arm = JS_ARM.replace("__ISO__", if iso.is_some() { "true" } else { "false" });
        // Resolve to an element in the same eval: a `ref` via XPath, or a
        // selector via `__find`, so a scripted action is one round trip.
        let resolve = locator_js(locator);
        let is_selector = matches!(locator, Locator::Selector { .. });
        let act = serde_json::to_string(action).unwrap_or_else(|_| "\"click\"".into());
        let val = serde_json::to_string(&value).unwrap_or_else(|_| "null".into());
        let real_input = !is_safari;

        // A click, submit or key press may start a navigation. Mark the
        // document it happens on, so a following `wait navigation` can tell
        // that document from the one it lands on.
        let nav_token =
            (!is_safari && matches!(action, "click" | "submit" | "press")).then(new_nav_token);
        let nav_mark = nav_token
            .as_ref()
            .map(|t| format!("window.__agentctl_nav_token = {t:?};"))
            .unwrap_or_default();
        // `wait navigation` reads the token from the page's world, which an
        // isolated-world script cannot write.
        let nav_mark_in_script = if iso.is_some() {
            if let (Some(c), false) = (c_opt.as_mut(), nav_mark.is_empty()) {
                Self::eval_value(c, &format!("{nav_mark} true")).await?;
            }
            String::new()
        } else {
            nav_mark
        };

        // `scroll_into_view` is the one action whose whole point is the
        // scroll, so it never skips it.
        let scroll = if action == "scroll_into_view" && opts.scroll == ScrollMode::None {
            ScrollMode::Nearest
        } else {
            opts.scroll
        };
        let scroll_js = |target: &str| {
            scroll
                .js_options()
                .map(|o| format!("try{{ {target}.scrollIntoView({o}); }}catch(e){{}}"))
                .unwrap_or_default()
        };
        let canvas_scroll = scroll_js("c");
        let el_scroll = scroll_js("el");

        // Install the request counters and mark "an action happens now" before
        // it does, so a request it starts (however late) is seen by the
        // settle waits. Best effort: a page that cannot be scripted just has
        // no counters.
        if let (Some(c), false) = (c_opt.as_mut(), action == "scroll_into_view") {
            let _ = Self::eval_value(c, &act_arm_js()).await;
        }

        let showcase_cfg = self
            .showcase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let showcase_engine = showcase_cfg.engine_js();

        // Real pointer movement (Chrome): the page gets trusted `mousemove`
        // events along the glide, so `:hover`, tooltips and mouse listeners
        // see the pointer arrive. A first script finds where the element is
        // (after scrolling it into view); the pointer is then moved there from
        // where it last was, while the drawn cursor glides alongside; the act
        // script that follows only does the click/type itself. `hover` always
        // moves the pointer, showcase or not. Canvas regions send their own
        // real input below.
        //
        // The recorder only listens for click/input/change/keydown, so these
        // `mouseMoved` events (no buttons, no click) are never recorded as
        // steps, also while a recording is running; no recording-specific
        // skip is needed.
        let mut real_move = false;
        if !is_safari && !canvas_ref && (action == "hover" || showcase_cfg.enabled) {
            let last = self
                .cursor_pos
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(target)
                .copied();
            let (engine_js, resume_js) = if showcase_cfg.enabled {
                let resume = last
                    .map(|(x, y)| {
                        format!(
                            "try{{ if(window.__agentctl_showcase) window.__agentctl_showcase.place({x},{y}); }}catch(e){{}}"
                        )
                    })
                    .unwrap_or_default();
                (showcase_guarded(&showcase_engine), resume)
            } else {
                (String::new(), String::new())
            };
            let probe = format!(
                r#"(async function(){{
  {JS_XPATH}
  {JS_FIND}
  {engine_js}
  {resume_js}
  var el;
  try {{ el = {resolve}; }} catch(e) {{ return null; }}
  if(!el || el.__is_canvas_target) return null;
  {el_scroll}
  var b = el.getBoundingClientRect();
  if(!(b.width > 0 || b.height > 0)) return null;
  return {{x: b.left + b.width / 2, y: b.top + b.height / 2}};
}})()"#
            );
            if let Some(c) = c_opt.as_mut() {
                let at = Self::eval_value_in(c, &probe, iso)
                    .await
                    .ok()
                    .and_then(|v| Some((v.get("x")?.as_f64()?, v.get("y")?.as_f64()?)));
                if let Some(to) = at {
                    let glide_ms = showcase_cfg.glide_ms();
                    let from = last.unwrap_or(if showcase_cfg.enabled { (0.0, 0.0) } else { to });
                    if showcase_cfg.enabled {
                        // The drawn cursor glides over the same time and curve.
                        let kick = format!(
                            "try{{ if(window.__agentctl_showcase){{ {} window.__agentctl_showcase.move({},{},{glide_ms}); }} }}catch(e){{}} true",
                            if last.is_none() {
                                format!("window.__agentctl_showcase.place({},{});", from.0, from.1)
                            } else {
                                String::new()
                            },
                            to.0,
                            to.1
                        );
                        let _ = Self::eval_value_in(c, &kick, iso).await;
                    }
                    real_move = Self::glide_mouse(c, from, to, glide_ms).await;
                    if real_move {
                        self.cursor_pos
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(target.to_string(), to);
                    }
                }
            }
        }

        let (showcase_init, showcase_call) = if showcase_cfg.enabled {
            // A glide that was already made with real mouse events leaves the
            // page-side cursor only the snap, ripple and HUD to do.
            let glide_ms = if real_move {
                0
            } else {
                showcase_cfg.glide_ms()
            };
            let ripple_ms = showcase_cfg.ripple_ms();
            let beat_ms = crate::showcase::ripple_beat_ms(ripple_ms);
            let typing_hud = showcase_cfg.typing_hud;
            (
                showcase_guarded(&showcase_engine),
                format!(
                    r#"
  try {{
    if(typeof window !== 'undefined' && window.__agentctl_showcase && typeof window.__agentctl_showcase.act === 'function') {{
      var b = el.getBoundingClientRect();
      var cx = Math.round(b.left + b.width / 2);
      var cy = Math.round(b.top + b.height / 2);
      await window.__agentctl_showcase.act(cx, cy, action, value, {glide_ms}, {ripple_ms}, {beat_ms}, {typing_hud}, el, {secret});
    }}
  }} catch(e) {{}}
"#
                ),
            )
        } else {
            (String::new(), String::new())
        };

        let expr = format!(
            r#"(async function(){{
  {JS_XPATH}
  {JS_FIND}
  {js_arm}
  {JS_SET_VALUE}
  {JS_TYPE_HELPERS}
  {showcase_init}
  var el, action={act}, value={val}, realMove={real_move}, realInput={real_input};
  var inputKind = null, inputReason = null, clickAt = null, insertText = false, readback = null;
  try {{ el = {resolve}; }} catch(e) {{ return {{ok:false,error:String(e && e.message ? e.message : e)}}; }}
  if(!el) return {{ok:false,error:'element not found'}};
  if(el.__is_canvas_target){{
    var c = el.canvas;
    if(action !== 'click' && action !== 'hover' && action !== 'scroll_into_view'){{
      return {{ok:false,kind:'unsupported',error:"action '"+action+"' is not supported on a canvas region (only click, hover, scroll_into_view); the region is drawn pixels, not a DOM element"}};
    }}
    {canvas_scroll}
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
  {el_scroll}
  {showcase_call}
  {nav_mark_in_script}
  var tgt = __target(el), found = {found};
  try {{
  switch(action){{
    case 'click': {{
      // A real click (Rust sends it once this script returns) fires the whole
      // pointer sequence, which `el.click()` does not: react-select opens its
      // menu on mousedown. It is only safe where the pointer would land on
      // this element, so it stays synthetic for Safari, native popups
      // (<select>, <option>), the file chooser, a child frame, an element with
      // no size or off screen, and one something else covers.
      var why = null, cpt = null;
      var ctag = (el.tagName || '').toLowerCase();
      if(!realInput) why = 'the Safari engine cannot send real pointer input';
      else if(ctag === 'option' || ctag === 'select') why = 'a native ' + ctag + ' control opens an OS popup';
      else if(ctag === 'input' && String(el.type).toLowerCase() === 'file') why = 'a real click on a file input opens the OS file chooser';
      else if(el.ownerDocument !== document) why = 'the element is inside a child frame';
      else {{
        var cb = el.getBoundingClientRect();
        var cx = cb.left + cb.width / 2, cy = cb.top + cb.height / 2;
        var cvw = document.documentElement.clientWidth, cvh = document.documentElement.clientHeight;
        if(!(cb.width > 0 && cb.height > 0)) why = 'the element has no size';
        else if(!(cx >= 0 && cy >= 0 && cx < cvw && cy < cvh)) why = 'the element centre is outside the viewport';
        else {{
          var croot = el.getRootNode ? el.getRootNode() : document;
          var ctop = (croot.elementFromPoint ? croot : document).elementFromPoint(cx, cy);
          if(!ctop) why = 'nothing is rendered at the element centre';
          else if(ctop === el || el.contains(ctop)) cpt = {{x: cx, y: cy}};
          else why = 'covered by <' + String(ctop.tagName || '?').toLowerCase() + '>';
        }}
      }}
      if(cpt){{ clickAt = cpt; inputKind = 'cdp'; }}
      else {{ inputKind = 'synthetic'; inputReason = why; __arm(el, 'click'); el.click(); }}
      break;
    }}
    case 'focus': el.focus({{preventScroll:true}}); break;
    case 'press': el.focus({{preventScroll:true}}); break;
    case 'hover': {{
      // The real pointer lands on whatever is on top at the element's
      // centre; when that is not the element (an overlay covers it), fall
      // back to the synthetic event so the element still hears the hover.
      var hb = el.getBoundingClientRect();
      var top = realMove ? document.elementFromPoint(hb.left + hb.width / 2, hb.top + hb.height / 2) : null;
      if(!top || !(top === el || el.contains(top))) el.dispatchEvent(new MouseEvent('mouseover',{{bubbles:true}}));
      break;
    }}
    case 'scroll_into_view': break;
    case 'submit':
      if(el.form){{ el.form.requestSubmit?el.form.requestSubmit():el.form.submit(); }}
      else if(typeof el.submit==='function'){{ el.submit(); }}
      else return {{ok:false,error:'element has no form to submit'}};
      break;
    case 'select':
      __setValue(el, value); __arm(el, 'change'); el.dispatchEvent(new Event('change',{{bubbles:true}})); break;
    case 'type':
      if(el.focus) el.focus({{preventScroll:true}});
      // A real, trusted insertion (Rust sends it once this script returns):
      // beforeinput and input fire as for a person typing, which React,
      // Lexical, ProseMirror and the like all take notice of. Empty text,
      // other element kinds and Safari set the value instead.
      if(realInput && value != null && String(value) !== '' && __canInsert(el)){{
        __selectContents(el);
        Object.defineProperty(window, '__agentctl_type_el', {{value: el, configurable: true, writable: true}});
        insertText = true;
        inputKind = 'cdp';
        break;
      }}
      if('value' in el){{ __setValue(el, value == null ? '' : String(value)); }} else {{ el.textContent=value; }}
      __arm(el, 'input');
      el.dispatchEvent(new Event('input',{{bubbles:true}}));
      __arm(el, 'change');
      el.dispatchEvent(new Event('change',{{bubbles:true}}));
      inputKind = 'synthetic';
      readback = __readback(el, {secret});
      break;
    default: return {{ok:false,error:'unknown action '+action}};
  }}
  }} finally {{ __disarm(); }}
  return {{ok:true,action:action,showcase:{}{showcase_rendered},input:inputKind,input_reason:inputReason,click_at:clickAt,type_insert:insertText,readback:readback,target:tgt,matches:found}};
}})()"#,
            showcase_cfg.enabled,
            found = if is_selector { "__find.count" } else { "null" },
            showcase_rendered = if showcase_cfg.enabled {
                format!(
                    ",showcase_rendered:{}",
                    crate::showcase::JS_SHOWCASE_RENDERED
                )
            } else {
                String::new()
            }
        );
        let mut v = if is_safari {
            let entry = self.get_safari_session(target)?;
            // The script is an async IIFE: `execute/sync` would not await it.
            entry.session.eval_promise(&expr).await?
        } else {
            let c = c_opt.as_mut().unwrap();
            Self::eval_value_in(c, &expr, iso).await?
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
        if let Some(token) = &nav_token {
            // The marker is on the document the action ran on; a click only
            // *might* navigate, so the wait for it is bounded (NAV_EXPECT_MS).
            self.set_nav_pending(target, token.clone(), false);
        }
        if let Some(at) = v.get("click_at").filter(|a| a.is_object()).cloned() {
            // The page found the element's centre uncovered: click it with
            // real, trusted pointer input. A dialog the click raises is
            // answered by the connection while it waits for the reply.
            if let Some(c) = c_opt.as_mut() {
                let (x, y) = (
                    at.get("x").and_then(Value::as_f64).unwrap_or(0.0),
                    at.get("y").and_then(Value::as_f64).unwrap_or(0.0),
                );
                Self::cdp_mouse(c, x, y, true).await?;
                self.cursor_pos
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(target.to_string(), (x, y));
            }
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
        if v.get("type_insert").and_then(Value::as_bool) == Some(true) {
            // The field is focused with its content selected; insert as real
            // input, then fire `change` (what leaving the field would do) and
            // read the field back.
            if let Some(c) = c_opt.as_mut() {
                c.call("Input.insertText", json!({ "text": value.unwrap_or("") }))
                    .await?;
                let finish = format!(
                    r#"(function(){{
  {js_arm}
  {JS_TYPE_HELPERS}
  var el = window.__agentctl_type_el;
  try {{ delete window.__agentctl_type_el; }} catch(e) {{}}
  if(!el) return null;
  try {{
    __arm(el, 'change');
    el.dispatchEvent(new Event('change', {{bubbles:true}}));
  }} finally {{ __disarm(); }}
  return __readback(el, {secret});
}})()"#
                );
                let after = Self::eval_value_in(c, &finish, iso).await?;
                if let Some(map) = v.as_object_mut() {
                    map.insert("readback".into(), after);
                }
            }
        }
        // Fold the page's report of how the input went into the result.
        if let Some(map) = v.as_object_mut() {
            map.remove("type_insert");
            map.remove("click_at");
            if map.get("input_reason").is_some_and(Value::is_null) {
                map.remove("input_reason");
            }
            if let Some(Value::Object(rb)) = map.remove("readback") {
                map.extend(rb);
            }
            if map.get("input").is_some_and(Value::is_null) {
                map.remove("input");
            }
            if map.get("matches").is_some_and(Value::is_null) {
                map.remove("matches");
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
        if opts.settle {
            drop(c_opt);
            let report = self
                .settle_after_act(target, nav_token, opts.timeout_ms)
                .await;
            if let (Some(m), Some(r)) = (v.as_object_mut(), report.as_object()) {
                m.extend(r.iter().map(|(k, x)| (k.clone(), x.clone())));
            }
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
        self.wait_window(target, cond, arg, timeout_ms, None).await
    }

    async fn wait_window(
        &self,
        target: &str,
        cond: &str,
        arg: Option<&str>,
        timeout_ms: u64,
        nav_window_ms: Option<u64>,
    ) -> Result<Value, BrowserError> {
        use tokio::time::{sleep, Duration, Instant};
        if target.starts_with("safari-") {
            let entry = self.get_safari_session(target)?;
            let deadline = Instant::now() + Duration::from_millis(timeout_ms.clamp(50, 60_000));
            if cond == "challenge_cleared" || cond == "challenge" {
                // Detection and the HUD run through `eval`, which the Safari
                // engine has.
                let res = crate::challenge::ChallengeManager::wait_for_clearance(
                    self, target, timeout_ms,
                )
                .await?;
                return Ok(json!({
                    "settled": true, "condition": cond, "challenge": res, "engine": "webkit"
                }));
            }
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
                "dom_settled" => safari_return(JS_DOM_SETTLED),
                "htmx_settled" => safari_return(&htmx_settled_js()),
                other => {
                    return Err(BrowserError::Failed(format!(
                        "unknown wait condition '{other}'"
                    )))
                }
            };
            loop {
                let hit = entry.session.execute_sync(&probe, &[]).await.map_err(|e| {
                    // The probe throws this when `window.htmx` is missing.
                    if cond == "htmx_settled" && err_msg(&e).contains("htmx not present") {
                        BrowserError::NotFound("htmx not present on page".into())
                    } else {
                        e
                    }
                })?;
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
                let expect_ms = nav_window_ms
                    .unwrap_or(NAV_EXPECT_MS)
                    .min(NAV_EXPECT_MAX_MS);
                return self
                    .wait_replaced_document(target, &mut c, pending, timeout_ms, expect_ms)
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
        // An action that may have navigated leaves the old document showing
        // for a moment, and the old document is as quiet as any: look at the
        // network only once the page has moved on (or the click has had its
        // NAV_EXPECT_MS to start navigating and did not).
        let mut navigated = None;
        if cond == "network_idle" {
            if let Some(pending) = self.nav_pending_for(target) {
                let left = deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis() as u64;
                let r = self
                    .wait_replaced_document(target, &mut c, pending, left, NAV_EXPECT_MS)
                    .await?;
                navigated = r.get("navigated").cloned();
            }
        }

        let probe = match cond {
            "selector" => {
                let s =
                    arg.ok_or_else(|| BrowserError::Failed("wait selector needs a value".into()))?;
                let sl = serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
                format!("!!document.querySelector({sl})")
            }
            "navigation" => "document.readyState==='complete'".to_string(),
            "network_idle" => net_idle_js(),
            "dom_settled" => JS_DOM_SETTLED.to_string(),
            "htmx_settled" => htmx_settled_js(),
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
                let mut out = json!({ "settled": true, "condition": cond });
                if let (Some(n), Some(m)) = (navigated, out.as_object_mut()) {
                    m.insert("navigated".into(), n);
                }
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
                .execute_sync(&safari_return(&expr), &[])
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

    async fn screencast_start(
        &self,
        target: &str,
        media_dir: &std::path::Path,
        opts: crate::screencast::ScreencastOpts,
    ) -> Result<Value, BrowserError> {
        require_cdp_target(target, "browser_screencast")?;
        let (b, _) = self.browser_ws_for_target(target).await?;
        self.screencasts
            .start((b.id, &b.host, b.port), target, media_dir, opts)
            .await
    }

    async fn screencast_stop(
        &self,
        target: Option<&str>,
        recording_id: Option<&str>,
        keep_frames: bool,
    ) -> Result<Value, BrowserError> {
        self.screencasts
            .stop(target, recording_id, keep_frames)
            .await
    }

    async fn screencast_status(&self) -> Result<Value, BrowserError> {
        Ok(self.screencasts.status())
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
            let t = expression.trim();
            let v = if t.starts_with("return ") || t.starts_with("function") {
                entry.session.execute_sync(expression, &[]).await?
            } else {
                // Same semantics as the Chrome path: the value of the last
                // statement, with a returned promise awaited.
                let mut env = entry
                    .session
                    .execute_async(&safari_eval_script(expression), &[])
                    .await?;
                if env.get("refused").and_then(Value::as_bool) == Some(true) {
                    // `eval` itself is refused (the page's CSP forbids it), and
                    // none of the code has run. The driver's own script
                    // body is not subject to it: try the code as an expression,
                    // and as a function body when it is not one.
                    env = match entry
                        .session
                        .execute_async(&safari_noeval_script(expression, true), &[])
                        .await
                    {
                        // The wrapper catches every runtime throw, so a driver
                        // "javascript error" here is the code failing to parse
                        // as an expression (Safari words it "Unexpected
                        // keyword ..."), not a result.
                        Err(e) if err_msg(&e).contains("javascript error") => {
                            entry
                                .session
                                .execute_async(&safari_noeval_script(expression, false), &[])
                                .await?
                        }
                        other => other?,
                    };
                }
                if env.get("ok").and_then(Value::as_bool) != Some(true) {
                    let msg = env
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("eval returned no result");
                    return Err(BrowserError::Failed(format!("eval: {msg}")));
                }
                env.get("value").cloned().unwrap_or(Value::Null)
            };
            return Ok(json!({ "result": v, "engine": "webkit" }));
        }

        let mut c = self.conn(target).await?;
        let v = Self::eval_value(&mut c, expression).await?;
        let mut out = json!({ "result": v });
        self.note_dialogs(target, &mut c, &mut out);
        Ok(out)
    }

    async fn eval_with(
        &self,
        target: &str,
        expression: &str,
        opts: &EvalOptions,
    ) -> Result<Value, BrowserError> {
        use tokio::time::Duration;
        if target.starts_with("safari-") {
            if opts.detached {
                return Err(BrowserError::Unsupported(
                    "detached eval needs the CDP (Chrome) engine; the WebKit engine cannot start a script and leave it running".into(),
                ));
            }
            let mut out = self.eval(target, expression).await?;
            if opts.timeout_ms.is_some() {
                out["timeout_note"] = json!(
                    "timeout_ms is not enforced on the WebKit engine; the script ran with the driver's own limit"
                );
            }
            return Ok(out);
        }

        let mut c = self.conn(target).await?;
        // Focus emulation lives and dies with this connection, so each
        // session that runs page script asks for it again.
        Self::emulate_focus(&mut c).await;
        c.call("Runtime.enable", json!({})).await.ok();

        let timeout_ms = clamp_eval_timeout(opts.timeout_ms);
        // The transport deadline is the limit. Chrome's own `timeout`
        // parameter would stop a synchronous loop too, but it reports it as an
        // opaque "Internal error"; `Runtime.terminateExecution` below does the
        // same job with a reply we can tell apart.
        let params = json!({
            "expression": expression,
            "returnByValue": true,
            // Detached: do not wait for a returned promise. The synchronous
            // part still runs inside this call (and under its time limit);
            // evaluating the code directly, rather than through a timer and
            // `eval`, keeps it working on pages whose CSP forbids `eval`.
            "awaitPromise": !opts.detached,
            "userGesture": true
        });
        let called = c
            .call_within(
                "Runtime.evaluate",
                params,
                Duration::from_millis(timeout_ms),
            )
            .await;
        let r = match called {
            Ok(r) => r,
            Err(BrowserError::Timeout(_)) => {
                let attempted = c
                    .call_within(
                        "Runtime.terminateExecution",
                        json!({}),
                        Duration::from_secs(2),
                    )
                    .await
                    .is_ok();
                return Err(BrowserError::Timeout(eval_timeout_message(
                    timeout_ms, attempted,
                )));
            }
            Err(BrowserError::Failed(m)) if is_navigated_message(&m) => {
                return Ok(json!({
                    "navigated": true,
                    "value": null,
                    "result": null,
                    "note": "the script navigated the page (or closed the tab) before it finished, so Chrome dropped its result; the script's own effects happened. Use browser_wait navigation / browser_snapshot to see the new page."
                }));
            }
            Err(e) => return Err(e),
        };
        if let Some(exc) = r.get("exceptionDetails") {
            let text = exc
                .get("exception")
                .and_then(|e| e.get("description").or_else(|| e.get("value")))
                .and_then(Value::as_str)
                .or_else(|| exc.get("text").and_then(Value::as_str))
                .unwrap_or("javascript error");
            return Err(BrowserError::Failed(format!("eval: {text}")));
        }
        if opts.detached {
            return Ok(json!({
                "started": true,
                "note": "the script's synchronous part has run; anything it left pending (promises, timers) carries on in the page and its result is not reported (a rejection goes to the page console)."
            }));
        }
        let v = r
            .get("result")
            .and_then(|o| o.get("value"))
            .cloned()
            .unwrap_or(Value::Null);
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
                    let expr = safari_return(JS_CAPTURE_HOOK);
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
                .execute_sync(&safari_return(&expr), &[])
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
        let iso = match c_opt.as_mut() {
            Some(c) => self.recorder_context(target, c).await,
            None => None,
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
            let ripple_ms = showcase_cfg.ripple_ms();
            let beat_ms = crate::showcase::ripple_beat_ms(ripple_ms);
            (
                showcase_guarded(&showcase_cfg.engine_js()),
                format!(
                    r#"
      try {{
        if(typeof window !== 'undefined' && window.__agentctl_showcase && typeof window.__agentctl_showcase.act === 'function') {{
          var fb = el.getBoundingClientRect();
          var fx = Math.round(fb.left + fb.width / 2);
          var fy = Math.round(fb.top + fb.height / 2);
          await window.__agentctl_showcase.act(fx, fy, 'type', val, {glide_ms}, 0, 0, true, el, f.secret === true);
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
          await window.__agentctl_showcase.act(sx, sy, 'click', null, {glide_ms}, {ripple_ms}, {beat_ms}, true);
        }}
      }} catch(e) {{}}
"#
                ),
            )
        } else {
            (String::new(), String::new(), String::new())
        };

        let expr = JS_FILL_FORM
            .replace("{JS_XPATH}", JS_XPATH)
            .replace(
                "{JS_ARM}",
                &JS_ARM.replace("__ISO__", if iso.is_some() { "true" } else { "false" }),
            )
            .replace("{JS_SET_VALUE}", JS_SET_VALUE)
            .replace("{JS_SHOWCASE_INIT}", &showcase_init)
            .replace("{JS_SHOWCASE_FIELD}", &showcase_field)
            .replace("{JS_SHOWCASE_SUBMIT}", &showcase_submit)
            .replace("__FIELDS__", &fields_json)
            .replace("__SUBMIT__", &submit_json);

        let mut v = if is_safari {
            let entry = self.get_safari_session(target)?;
            entry.session.eval_promise(&expr).await?
        } else {
            let c = c_opt.as_mut().unwrap();
            Self::eval_value_in(c, &expr, iso).await?
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
                .execute_sync(&safari_return(&expr), &[])
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
        require_cdp_target(target_id, "branch_create")?;
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
            self.goto_and_wait(&branch.parent_target_id, &final_url, 15_000, false)
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
        require_cdp_target(target_id, "checkpoint_save")?;
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
        require_cdp_target(target_id, "checkpoint_rollback")?;
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
        // The page is loaded from the network, not Chrome's cache: a cached
        // copy can differ from the server's state (or outlive the server), and
        // a rollback that reports success for it would be vouching for a page
        // nobody just fetched.
        if navigated {
            self.goto_and_wait(target_id, &cp.url, 15_000, true)
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
  {JS_SET_VALUE}
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
        __setChecked(el, !!item.checked);
      }} else if (item.value !== undefined && item.value !== null) {{
        __setValue(el, typeof item.value === 'string' ? item.value : JSON.stringify(item.value));
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
            "cache_bypassed": navigated,
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
        let mut out = cfg.to_json();
        if !cfg.enabled {
            self.cursor_pos
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
            return Ok(out);
        }
        // Say whether the overlay exists in the page, not just that it was
        // asked for: this is main-world, so `window.__agentctl_showcase` is
        // the same object a page script would see.
        let (rendered, problem) = if target.is_empty() {
            (
                false,
                Some("no target_id: the overlay is only drawn into a page, and acts draw it when they run".to_string()),
            )
        } else {
            let script = showcase_install_script(&cfg);
            let probed = if target.starts_with("safari-") {
                match self.get_safari_session(target) {
                    Ok(entry) => {
                        entry
                            .session
                            .execute_sync(&safari_return(&script), &[])
                            .await
                    }
                    Err(e) => Err(e),
                }
            } else {
                match self.conn(target).await {
                    Ok(mut c) => Self::eval_value(&mut c, &script).await,
                    Err(e) => Err(e),
                }
            };
            match probed {
                Ok(v) if v.get("rendered").and_then(Value::as_bool) == Some(true) => (true, None),
                Ok(v) => (
                    false,
                    Some(
                        v.get("reason")
                            .and_then(Value::as_str)
                            .unwrap_or("the overlay was not created")
                            .to_string(),
                    ),
                ),
                Err(e) => (
                    false,
                    Some(format!("overlay injection failed: {}", err_msg(&e))),
                ),
            }
        };
        if let Some(m) = out.as_object_mut() {
            m.insert("rendered".into(), json!(rendered));
            if let Some(w) = problem {
                tracing::warn!("showcase overlay not rendered: {w}");
                m.insert(
                    "warning".into(),
                    json!(format!(
                        "showcase is enabled but the cursor overlay is not on the page: {w}"
                    )),
                );
            }
        }
        Ok(out)
    }
}

/// The script `browser_showcase` runs in a page: install the overlay, then
/// report whether it is really there (`{rendered, reason?}`).
fn showcase_install_script(cfg: &crate::showcase::ShowcaseConfig) -> String {
    format!(
        r#"(function(){{
  if(!document || !document.body) return {{rendered:false, reason:'the page has no <body> yet (still loading, or not an HTML page)'}};
  try {{ {engine} }} catch(e) {{ return {{rendered:false, reason:'overlay script threw: ' + String(e && e.message ? e.message : e)}}; }}
  return {check} ? {{rendered:true}} : {{rendered:false, reason:'the overlay script ran but created no cursor element'}};
}})()"#,
        engine = cfg.engine_js(),
        check = crate::showcase::JS_SHOWCASE_RENDERED
    )
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

/// The temp profile directory this process creates for its `seq`-th launch.
///
/// Keyed on the pid as well as a per-process launch counter (the port is not
/// known until Chrome has started) so two concurrent servers never share a
/// profile, and so ownership is decidable: only a directory matching this
/// shape, for *our* pid, was created by us and may be deleted.
fn own_profile_dir(seq: u64) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("agentctl-cdp-{}-{seq}", std::process::id()))
}

/// Parse the first line of Chrome's `DevToolsActivePort` file, which holds the
/// port it actually bound. Returns `None` for a missing, partial or non-port
/// first line (a reader can catch the file empty), and for `0`, which is never
/// a bound port.
fn parse_devtools_active_port(contents: &str) -> Option<u16> {
    contents
        .lines()
        .next()?
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|p| *p != 0)
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

/// `browser_eval`'s `timeout_ms` as it will be applied.
pub(crate) fn clamp_eval_timeout(requested: Option<u64>) -> u64 {
    requested
        .unwrap_or(EVAL_TIMEOUT_DEFAULT_MS)
        .clamp(EVAL_TIMEOUT_MIN_MS, EVAL_TIMEOUT_MAX_MS)
}

/// Chrome's reply when the script navigated the page (or closed the tab)
/// while `Runtime.evaluate` was still waiting for it.
pub(crate) fn is_navigated_message(msg: &str) -> bool {
    msg.contains("Inspected target navigated or closed")
}

/// What a timed-out eval reports. Termination only reaches script that is
/// running right now, not one that is waiting, so the message does not claim
/// more than that.
pub(crate) fn eval_timeout_message(timeout_ms: u64, terminate_sent: bool) -> String {
    let what = if terminate_sent {
        "Runtime.terminateExecution was sent (it stops script that is running, such as a loop, not one that is waiting)"
    } else {
        "Runtime.terminateExecution could not be sent"
    };
    format!(
        "browser_eval timed out after {timeout_ms} ms; {what}. Async work the script already \
         scheduled (timers, pending promises, event handlers) may still be running in the page \
         and can interleave with later calls; reload the tab if that matters"
    )
}

/// Flags `launch.args` may pass, names only (without the leading `--`). An
/// allowlist, not a denylist: Chrome has flags that run a program of the
/// caller's choosing (`--renderer-cmd-prefix`, `--gpu-launcher`,
/// `--browser-subprocess-path`), open the DevTools socket beyond loopback, or
/// switch off the sandbox, site isolation, TLS checks or the navigation
/// policy, and new ones arrive with each release. `browser_connect` is
/// standard tier, so it may only reach flags that change how the browser looks
/// and paces itself.
pub(crate) const ALLOWED_LAUNCH_FLAGS: &[&str] = &[
    "window-size",
    "window-position",
    "start-maximized",
    "start-fullscreen",
    "force-device-scale-factor",
    "hide-scrollbars",
    "force-dark-mode",
    "lang",
    "accept-lang",
    "user-agent",
    "mute-audio",
    "autoplay-policy",
    "disable-gpu",
    "disable-extensions",
    "disable-notifications",
    "disable-default-apps",
    "disable-sync",
    "disable-search-engine-choice-screen",
    "use-fake-device-for-media-stream",
    "auto-open-devtools-for-tabs",
    "incognito",
    "disable-backgrounding-occluded-windows",
    "disable-renderer-backgrounding",
    "disable-background-timer-throttling",
    "disable-features",
];

/// Features `--disable-features` may name. Restricted for the same reason as
/// the flags: the feature list also reaches site isolation and private
/// network protections (`--disable-features=IsolateOrigins,site-per-process`).
pub(crate) const ALLOWED_DISABLED_FEATURES: &[&str] = &[
    "CalculateNativeWinOcclusion",
    "Translate",
    "MediaRouter",
    "OptimizationHints",
    "AutofillServerCommunication",
    "PaintHolding",
];

const MAX_LAUNCH_ARGS: usize = 32;
const MAX_LAUNCH_ARG_LEN: usize = 256;

/// Validate `launch.args`: an array of at most 32 strings, each a `--flag` or
/// `--flag=value` with no whitespace or control characters, each on
/// [`ALLOWED_LAUNCH_FLAGS`]. Returns them as given.
pub(crate) fn validate_launch_args(args: &Value) -> Result<Vec<String>, String> {
    let Some(arr) = args.as_array() else {
        return Err("launch.args must be an array of strings".into());
    };
    if arr.len() > MAX_LAUNCH_ARGS {
        return Err(format!(
            "launch.args has {} entries; at most {MAX_LAUNCH_ARGS} are allowed",
            arr.len()
        ));
    }
    let mut out = Vec::with_capacity(arr.len());
    for v in arr {
        let Some(a) = v.as_str() else {
            return Err("launch.args must be an array of strings".into());
        };
        if a.len() > MAX_LAUNCH_ARG_LEN {
            return Err(format!(
                "launch.args entry is {} bytes; at most {MAX_LAUNCH_ARG_LEN} are allowed",
                a.len()
            ));
        }
        if a.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
            return Err(format!(
                "launch.args entry {a:?} contains whitespace or a control character; give one flag per entry, as --name=value"
            ));
        }
        let Some(body) = a.strip_prefix("--") else {
            return Err(format!("launch.args entry {a:?} must start with --"));
        };
        let name = body.split('=').next().unwrap_or("").to_ascii_lowercase();
        if name.is_empty()
            || !name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
        {
            return Err(format!("launch.args entry {a:?} is not a valid --flag"));
        }
        if !ALLOWED_LAUNCH_FLAGS.contains(&name.as_str()) {
            return Err(format!(
                "launch.args flag --{name} is not allowed; permitted: --{}",
                ALLOWED_LAUNCH_FLAGS.join(", --")
            ));
        }
        if name == "disable-features" {
            let list = body.split_once('=').map(|(_, v)| v).unwrap_or("");
            if let Some(f) = list
                .split(',')
                .find(|f| !f.is_empty() && !ALLOWED_DISABLED_FEATURES.contains(f))
            {
                return Err(format!(
                    "launch.args --disable-features may not name {f:?}; permitted: {}",
                    ALLOWED_DISABLED_FEATURES.join(", ")
                ));
            }
        }
        out.push(a.to_string());
    }
    Ok(out)
}

/// Flags that keep a headed Chrome running at full speed when its window is
/// covered or behind another (otherwise timers throttle, `visibilityState`
/// goes `hidden`, and screen recordings freeze).
const FOREGROUND_FLAGS: &[&str] = &[
    "--disable-backgrounding-occluded-windows",
    "--disable-renderer-backgrounding",
    "--disable-background-timer-throttling",
];
const FOREGROUND_DISABLED_FEATURES: &[&str] = &["CalculateNativeWinOcclusion"];

/// The full Chrome command line (without the binary). `extra` must already
/// have passed [`validate_launch_args`]. Any `--disable-features` among them is
/// merged with ours into a single flag, since Chrome keeps only the last.
pub(crate) fn chrome_launch_flags(
    port: u16,
    user_data_dir: &str,
    headless: bool,
    background_throttling: bool,
    extra: &[String],
) -> Vec<String> {
    let mut flags = vec![
        format!("--remote-debugging-port={port}"),
        format!("--user-data-dir={user_data_dir}"),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
    ];
    let mut features: Vec<String> = Vec::new();
    if headless {
        flags.push("--headless=new".to_string());
    } else if !background_throttling {
        flags.extend(FOREGROUND_FLAGS.iter().map(|f| f.to_string()));
        features.extend(FOREGROUND_DISABLED_FEATURES.iter().map(|f| f.to_string()));
    }
    for a in extra {
        match a.strip_prefix("--disable-features=") {
            Some(list) => features.extend(
                list.split(',')
                    .filter(|f| !f.is_empty())
                    .map(str::to_string),
            ),
            None => flags.push(a.clone()),
        }
    }
    let mut seen = std::collections::HashSet::new();
    features.retain(|f| seen.insert(f.clone()));
    if !features.is_empty() {
        flags.push(format!("--disable-features={}", features.join(",")));
    }
    flags
}

/// Launch a dedicated Chromium instance with a debugging port and poll until
/// its CDP endpoint answers.
///
/// Returns the child handle so the caller can stop it again: a dropped
/// `Child` does **not** kill the process, so discarding it leaks a browser and
/// its profile directory for the life of the machine.
type Launch = (String, u16, std::process::Child, Option<std::path::PathBuf>);

/// Distinguishes the profile directories of launches inside one process, now
/// that the port is unknown until Chrome has started.
static PROFILE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

async fn launch_browser(spec: &Value) -> Result<Launch, BrowserError> {
    use tokio::time::{sleep, Duration};
    // An explicit port is honoured (e.g. to attach DevTools by hand). Omitted or
    // 0 means "let Chrome choose": it binds an ephemeral port itself and reports
    // it in `DevToolsActivePort`, so there is no pick-then-release window for
    // another process to take the port in, and back-to-back launches never collide.
    let requested_port = match spec.get("port").and_then(Value::as_u64) {
        None | Some(0) => 0,
        Some(p) => u16::try_from(p).map_err(|_| {
            BrowserError::Failed(format!(
                "launch.port {p} is not a valid TCP port (0 or 1-65535)"
            ))
        })?,
    };
    let headless = spec
        .get("headless")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let extra_args = match spec.get("args") {
        None | Some(Value::Null) => Vec::new(),
        Some(a) => validate_launch_args(a).map_err(BrowserError::Failed)?,
    };
    let background_throttling = spec
        .get("background_throttling")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // Only a directory we chose is ours to delete later.
    let (user_data_dir, owned) = match spec.get("user_data_dir").and_then(Value::as_str) {
        Some(p) => (std::path::PathBuf::from(p), None),
        None => {
            let d = own_profile_dir(PROFILE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
            (d.clone(), Some(d))
        }
    };
    // A profile reused from an earlier run still holds that run's port; Chrome
    // rewrites the file once it is listening, so clear it or we would read the
    // stale one.
    let active_port_file = user_data_dir.join("DevToolsActivePort");
    let _ = std::fs::remove_file(&active_port_file);
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
    cmd.args(chrome_launch_flags(
        requested_port,
        &user_data_dir,
        headless,
        background_throttling,
        &extra_args,
    ));
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let child = cmd
        .spawn()
        .map_err(|e| BrowserError::Failed(format!("spawn {bin}: {e}")))?;

    // Poll for readiness (~8s): first learn the port, then wait for CDP.
    let mut port = (requested_port != 0).then_some(requested_port);
    for _ in 0..40 {
        if port.is_none() {
            port = std::fs::read_to_string(&active_port_file)
                .ok()
                .and_then(|c| parse_devtools_active_port(&c));
        }
        if let Some(p) = port {
            if http_json("127.0.0.1", p, "GET", "/json/version")
                .await
                .is_ok()
            {
                return Ok(("127.0.0.1".to_string(), p, child, owned));
            }
        }
        sleep(Duration::from_millis(200)).await;
    }
    // It never came up, so nothing else will ever hold this handle.
    reap_one(child, owned.as_deref());
    Err(BrowserError::Timeout(match port {
        Some(p) => format!("launched browser but CDP port {p} never came up"),
        None => "launched browser but it never reported a DevTools port".to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_timeout_is_defaulted_and_clamped() {
        assert_eq!(clamp_eval_timeout(None), 10_000);
        assert_eq!(clamp_eval_timeout(Some(0)), 100);
        assert_eq!(clamp_eval_timeout(Some(5_000)), 5_000);
        assert_eq!(clamp_eval_timeout(Some(u64::MAX)), 60_000);
    }

    #[test]
    fn eval_chrome_messages_are_recognised() {
        assert!(is_navigated_message(
            "Runtime.evaluate: Inspected target navigated or closed"
        ));
        assert!(!is_navigated_message("Runtime.evaluate: something else"));
        let m = eval_timeout_message(250, true);
        assert!(m.contains("timed out after 250 ms"));
        assert!(m.contains("terminateExecution was sent"));
        assert!(m.contains("may still be running"));
        assert!(eval_timeout_message(250, false).contains("could not be sent"));
    }

    #[test]
    fn launch_args_accept_plain_flags() {
        let ok = validate_launch_args(&json!([
            "--lang=fr",
            "--mute-audio",
            "--window-size=800,600"
        ]))
        .unwrap();
        assert_eq!(ok.len(), 3);
        assert!(validate_launch_args(&json!([])).unwrap().is_empty());
    }

    #[test]
    fn launch_args_reject_malformed_entries() {
        for bad in [
            json!("--a"),
            json!([1]),
            json!(["lang=fr"]),
            json!(["-x"]),
            json!(["--"]),
            json!(["--=x"]),
            json!(["--lang fr"]),
            json!(["--lang=fr\n--no-sandbox"]),
            json!(["--a\tb"]),
            json!(["--a_b"]),
            json!([format!("--{}", "a".repeat(300))]),
        ] {
            assert!(validate_launch_args(&bad).is_err(), "{bad}");
        }
        let many: Vec<String> = (0..33).map(|i| format!("--lang=l{i}")).collect();
        assert!(validate_launch_args(&json!(many)).is_err());
        let max: Vec<String> = (0..32).map(|i| format!("--lang=l{i}")).collect();
        assert_eq!(validate_launch_args(&json!(max)).unwrap().len(), 32);
    }

    #[test]
    fn launch_args_refuse_flags_off_the_allowlist() {
        // Flags that run a program, widen the DevTools socket, or drop a
        // protection; any spelling.
        for name in [
            "renderer-cmd-prefix",
            "gpu-launcher",
            "utility-cmd-prefix",
            "browser-subprocess-path",
            "remote-debugging-address",
            "remote-debugging-port",
            "remote-allow-origins",
            "user-data-dir",
            "no-sandbox",
            "disable-web-security",
            "ignore-certificate-errors",
            "load-extension",
            "app",
            "proxy-server",
            "host-resolver-rules",
            "enable-features",
        ] {
            for spelled in [
                format!("--{name}"),
                format!("--{name}=x"),
                format!("--{}=x", name.to_uppercase()),
            ] {
                assert!(
                    validate_launch_args(&json!([spelled])).is_err(),
                    "{spelled}"
                );
            }
        }
        let e = validate_launch_args(&json!(["--gpu-launcher=/tmp/x"])).unwrap_err();
        assert!(e.contains("gpu-launcher") && e.contains("--lang"), "{e}");
        for name in ALLOWED_LAUNCH_FLAGS {
            assert!(validate_launch_args(&json!([format!("--{name}")])).is_ok());
        }
    }

    #[test]
    fn disable_features_is_limited_to_harmless_features() {
        assert!(validate_launch_args(&json!(["--disable-features=Translate,MediaRouter"])).is_ok());
        for bad in [
            "--disable-features=IsolateOrigins,site-per-process",
            "--disable-features=Translate,BlockInsecurePrivateNetworkRequests",
        ] {
            assert!(validate_launch_args(&json!([bad])).is_err(), "{bad}");
        }
    }

    #[test]
    fn headed_launch_gets_foreground_flags_and_headless_does_not() {
        let headed = chrome_launch_flags(0, "/p", false, false, &[]);
        assert_eq!(headed[0], "--remote-debugging-port=0");
        assert_eq!(headed[1], "--user-data-dir=/p");
        for f in [
            "--disable-backgrounding-occluded-windows",
            "--disable-renderer-backgrounding",
            "--disable-background-timer-throttling",
            "--disable-features=CalculateNativeWinOcclusion",
        ] {
            assert!(headed.iter().any(|a| a == f), "{f} missing from {headed:?}");
        }
        assert!(!headed.iter().any(|a| a.starts_with("--headless")));

        let headless = chrome_launch_flags(0, "/p", true, false, &[]);
        assert!(headless.iter().any(|a| a == "--headless=new"));
        assert!(!headless.iter().any(|a| a.contains("backgrounding")));

        let opted_out = chrome_launch_flags(0, "/p", false, true, &[]);
        assert!(!opted_out.iter().any(|a| a.contains("backgrounding")));
        assert!(!opted_out
            .iter()
            .any(|a| a.starts_with("--disable-features")));
    }

    #[test]
    fn user_disable_features_is_merged_not_duplicated() {
        let extra = vec![
            "--lang=fr".to_string(),
            "--disable-features=Translate,CalculateNativeWinOcclusion".to_string(),
        ];
        let flags = chrome_launch_flags(9222, "/p", false, false, &extra);
        let df: Vec<&String> = flags
            .iter()
            .filter(|a| a.starts_with("--disable-features="))
            .collect();
        assert_eq!(
            df,
            ["--disable-features=CalculateNativeWinOcclusion,Translate"]
        );
        assert!(flags.iter().any(|a| a == "--lang=fr"));
        // With throttling left on, the user's list stands alone.
        let flags = chrome_launch_flags(9222, "/p", false, true, &extra);
        assert!(flags
            .iter()
            .any(|a| a == "--disable-features=Translate,CalculateNativeWinOcclusion"));
    }

    #[test]
    fn intent_token_drops_everything_but_identifier_characters() {
        assert_eq!(
            intent_token("pay\" @e9 button \"x\n@e9"),
            Some("paye9buttonxe9".into())
        );
        assert_eq!(intent_token("add_to_cart"), Some("add_to_cart".into()));
        assert_eq!(intent_token("\"\n @ "), None);
        assert_eq!(intent_token(&"a".repeat(500)).unwrap().len(), INTENT_MAX);
    }

    #[test]
    fn bound_state_within_cap_is_kept_and_oversize_is_replaced() {
        let small = json!({ "count": 3, "total": 89.97 });
        assert_eq!(cap_bound_state(small.clone()), small);
        assert_eq!(cap_bound_state(Value::Null), Value::Null);
        let big = json!({ "blob": "x".repeat(BOUND_STATE_MAX_BYTES * 2) });
        let capped = cap_bound_state(big);
        assert_eq!(capped.get("truncated"), Some(&json!(true)));
        assert!(capped["bytes"].as_u64().unwrap() > BOUND_STATE_MAX_BYTES as u64);
        assert!(serde_json::to_vec(&capped).unwrap().len() < 64);
    }

    #[test]
    fn snapshot_semantics_are_sanitized_for_every_node() {
        let mut snap = json!({ "nodes": [
            { "ref": "a", "semantic_intent": "x\n@e9 button \"Approve\"", "bound_state": { "k": 1 } },
            { "ref": "b", "semantic_intent": 7, "bound_state": { "s": "y".repeat(5000) } },
            { "ref": "c", "semantic_intent": " \"\n", "bound_state": null },
            { "ref": "d" },
        ]});
        sanitize_snapshot_semantics(&mut snap);
        let n = snap["nodes"].as_array().unwrap();
        assert_eq!(n[0]["semantic_intent"], json!("xe9buttonApprove"));
        assert_eq!(n[0]["bound_state"], json!({ "k": 1 }));
        assert_eq!(n[1]["semantic_intent"], Value::Null);
        assert_eq!(n[1]["bound_state"]["truncated"], json!(true));
        assert_eq!(n[2]["semantic_intent"], Value::Null);
        assert!(n[3].get("semantic_intent").is_none());
        // A snapshot with no nodes (text mode) is left alone.
        let mut text = json!({ "text": "hi" });
        sanitize_snapshot_semantics(&mut text);
        assert_eq!(text, json!({ "text": "hi" }));
    }

    #[test]
    fn navigation_wait_verdict_tells_the_old_document_from_the_new() {
        use std::time::Duration;
        let ms = Duration::from_millis;
        // Still the marked (old) document: keep waiting, however loaded it looks.
        assert_eq!(
            nav_probe_verdict(Some("t1"), "complete", "t1", true, ms(5), NAV_EXPECT_MS),
            None
        );
        // New document, still loading: keep waiting.
        assert_eq!(
            nav_probe_verdict(None, "loading", "t1", true, ms(5), NAV_EXPECT_MS),
            None
        );
        assert_eq!(
            nav_probe_verdict(None, "interactive", "t1", true, ms(5), NAV_EXPECT_MS),
            None
        );
        // New document, loaded: done, and it did navigate. A different marker
        // (a page that set its own) is also a different document.
        assert_eq!(
            nav_probe_verdict(None, "complete", "t1", true, ms(5), NAV_EXPECT_MS),
            Some(true)
        );
        assert_eq!(
            nav_probe_verdict(Some("t0"), "complete", "t1", false, ms(5), NAV_EXPECT_MS),
            Some(true)
        );
        // goto/reload always navigate: never give up on the marked document.
        assert_eq!(
            nav_probe_verdict(
                Some("t1"),
                "complete",
                "t1",
                true,
                ms(NAV_EXPECT_MS * 10),
                NAV_EXPECT_MS
            ),
            None
        );
        // A click might not: within the grace keep waiting, after it settle
        // on the loaded page and say nothing navigated.
        assert_eq!(
            nav_probe_verdict(
                Some("t1"),
                "complete",
                "t1",
                false,
                ms(NAV_EXPECT_MS - 1),
                NAV_EXPECT_MS
            ),
            None
        );
        assert_eq!(
            nav_probe_verdict(
                Some("t1"),
                "complete",
                "t1",
                false,
                ms(NAV_EXPECT_MS),
                NAV_EXPECT_MS
            ),
            Some(false)
        );
        // ...but never while the marked document is still loading.
        assert_eq!(
            nav_probe_verdict(
                Some("t1"),
                "loading",
                "t1",
                false,
                ms(NAV_EXPECT_MS * 5),
                NAV_EXPECT_MS
            ),
            None
        );
    }

    /// The window a click's navigation is expected within is the caller's.
    #[test]
    fn nav_probe_verdict_uses_the_callers_window() {
        let ms = std::time::Duration::from_millis;
        assert_eq!(
            nav_probe_verdict(Some("t1"), "complete", "t1", false, ms(4_999), 5_000),
            None
        );
        assert_eq!(
            nav_probe_verdict(Some("t1"), "complete", "t1", false, ms(5_000), 5_000),
            Some(false)
        );
        // A zero window gives up as soon as the document has loaded.
        assert_eq!(
            nav_probe_verdict(Some("t1"), "complete", "t1", false, ms(0), 0),
            Some(false)
        );
        // Whatever the window, a replaced document still wins.
        assert_eq!(
            nav_probe_verdict(None, "complete", "t1", false, ms(1), 5_000),
            Some(true)
        );
    }

    #[test]
    fn settle_gives_up_on_a_navigation_once_the_page_works_in_place() {
        let ms = std::time::Duration::from_millis;
        // A request began on the unreplaced, loaded document: not navigating.
        assert_eq!(
            settle_nav_verdict(Some("t1"), "complete", "t1", 1, ms(5), NAV_EXPECT_MS),
            Some(false)
        );
        // No request yet and the window still open: keep waiting.
        assert_eq!(
            settle_nav_verdict(Some("t1"), "complete", "t1", 0, ms(5), NAV_EXPECT_MS),
            None
        );
        // Still loading: a request does not decide anything.
        assert_eq!(
            settle_nav_verdict(Some("t1"), "loading", "t1", 3, ms(5), NAV_EXPECT_MS),
            None
        );
        // The document was replaced: that wins over any count.
        assert_eq!(
            settle_nav_verdict(None, "complete", "t1", 2, ms(5), NAV_EXPECT_MS),
            Some(true)
        );
        // Nothing happened for the whole window.
        assert_eq!(
            settle_nav_verdict(
                Some("t1"),
                "complete",
                "t1",
                0,
                ms(NAV_EXPECT_MS),
                NAV_EXPECT_MS
            ),
            Some(false)
        );
    }

    #[test]
    fn scroll_modes_map_to_scroll_into_view_options() {
        assert_eq!(ScrollMode::default(), ScrollMode::Nearest);
        assert_eq!(ScrollMode::None.js_options(), None);
        let nearest = ScrollMode::Nearest.js_options().unwrap();
        assert!(nearest.contains("block:'nearest'") && nearest.contains("inline:'nearest'"));
        let center = ScrollMode::Center.js_options().unwrap();
        assert!(center.contains("block:'center'") && center.contains("inline:'center'"));
        // A position read right after must not catch a smooth scroll mid-way.
        assert!(nearest.contains("behavior:'instant'") && center.contains("behavior:'instant'"));
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
        for (name, vk) in [
            ("ArrowLeft", 37),
            ("ArrowUp", 38),
            ("ArrowRight", 39),
            ("ArrowDown", 40),
            ("PageUp", 33),
            ("PageDown", 34),
            ("End", 35),
            ("Home", 36),
            ("Backspace", 8),
            ("Delete", 46),
        ] {
            let k = key_event_spec(name).unwrap_or_else(|| panic!("{name} unsupported"));
            assert_eq!(
                (k.key, k.code, k.vk, k.text),
                (name, name, vk, None),
                "{name}"
            );
        }
        // Space is the one new key that types: both spellings name it.
        for name in ["Space", " "] {
            let k = key_event_spec(name).unwrap();
            assert_eq!((k.key, k.code, k.vk, k.text), (" ", "Space", 32, Some(" ")));
        }
        assert!(key_event_spec("F13").is_none());
        assert!(key_event_spec("").is_none());
    }

    /// Only a directory *we* named is ours to delete. The pid is in the name
    /// so two servers never share a profile, and so "did we create this?" is
    /// decidable from the path alone rather than from a guess about the port.
    #[test]
    fn own_profile_dir_is_pid_and_launch_scoped() {
        let a = own_profile_dir(1);
        let b = own_profile_dir(2);
        assert_ne!(a, b, "different launches get different profiles");
        assert!(a.starts_with(std::env::temp_dir()));
        let name = a.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(
            name,
            format!("agentctl-cdp-{}-1", std::process::id()),
            "the pid must be in the name"
        );
    }

    /// Chrome writes the bound port on the first line of `DevToolsActivePort`
    /// and the browser websocket path on the second; only the port matters.
    #[test]
    fn devtools_active_port_reads_first_line() {
        assert_eq!(
            parse_devtools_active_port("40123\n/devtools/browser/abc-def\n"),
            Some(40123)
        );
        assert_eq!(parse_devtools_active_port("9222"), Some(9222));
        assert_eq!(parse_devtools_active_port("  65535 \r\nx"), Some(65535));
        // Missing, half-written or nonsense contents are "not ready yet".
        assert_eq!(parse_devtools_active_port(""), None);
        assert_eq!(parse_devtools_active_port("\n/devtools/browser/x"), None);
        assert_eq!(parse_devtools_active_port("abc\n"), None);
        assert_eq!(parse_devtools_active_port("0\n"), None);
        assert_eq!(parse_devtools_active_port("65536\n"), None);
        assert_eq!(parse_devtools_active_port("-1\n"), None);
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
            None => Some(own_profile_dir(9)),
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

    #[test]
    fn cdp_only_features_refuse_a_safari_target_explicitly() {
        assert!(matches!(
            require_cdp_target("safari-1", "branch_create"),
            Err(BrowserError::Unsupported(m)) if m.contains("branch_create")
        ));
        assert!(require_cdp_target("ABC123", "branch_create").is_ok());
    }

    #[test]
    fn safari_return_is_not_defeated_by_a_leading_newline() {
        assert_eq!(
            safari_return("\n  (function(){})()\n"),
            "return (function(){})();"
        );
        assert_eq!(safari_return("1"), "return 1;");
    }

    #[test]
    fn safari_eval_probes_eval_before_running_the_expression() {
        let s = safari_eval_script("buy()");
        let probe = s.find("(0, eval)('0')").expect("probe");
        let run = s.find(r#"(0, eval)("buy()")"#).expect("run");
        assert!(probe < run, "{s}");
        assert!(s.contains("refused:true"), "{s}");
    }

    #[test]
    fn safari_noeval_script_shapes() {
        let e = safari_noeval_script("1 + 1 // c", true);
        assert!(e.contains("Promise.resolve((1 + 1 // c\n))"), "{e}");
        assert!(!e.contains("eval"));
        let f = safari_noeval_script("a(); return 2", false);
        assert!(
            f.contains("(function(){\na(); return 2\n}).call(window)"),
            "{f}"
        );
    }

    #[test]
    fn safari_eval_script_embeds_the_expression_as_a_string_literal() {
        let s = safari_eval_script("a(); \"q\"\n+ 1");
        assert!(s.contains(r#"(0, eval)("a(); \"q\"\n+ 1")"#), "{s}");
        assert!(s.contains("Promise.resolve"));
    }
}
