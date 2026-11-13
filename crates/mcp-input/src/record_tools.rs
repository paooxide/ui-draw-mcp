//! `desktop_record` and `desktop_flow`: the tools around [`crate::record`].
//!
//! Recording is a bounded, explicit session. It listens to what a person does
//! (never acts), keeps only events whose application is in `allowed_apps`
//! (the others are counted and dropped, not stored), scrubs the text of secure
//! fields the moment it is read, stops by itself at a time limit and an event
//! limit, and leaves a marker file in the state directory while it runs so
//! `agentctl doctor` can say so.
//!
//! Replay never acts on its own. Each step is resolved against the live tree
//! and then handed to the server's gate as the ordinary tool call it stands for
//! (`ui_action`, `keyboard_type`, `keyboard_shortcut`, `set_value`,
//! `mouse_action`), through the same [`StepDispatch`] `input_steps` uses, so
//! the tier, role, consent, foreground mode, effects and audit of that tool all
//! apply to every step. The first step that fails stops the run.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use mcp_a11y::{flatten, BackendError, FlattenConfig, SnapshotRequest};
use mcp_types::{CallCtx, Envelope, ErrorCode};
use serde_json::{json, Value};

use crate::record::{
    admit, candidates_of, click_action, missing_secrets, reduce, resolve, scrub, secret_refs,
    validate_secrets, ActiveMarker, Admit, Captured, DesktopFlow, FlowError, FlowStore, MatchKind,
    Op, RecordBackend, RecordError, RecordSession, Resolution, Step, DEFAULT_RECORD_MS,
    MAX_RAW_EVENTS, MAX_RECORD_MS, MAX_RECORD_STEPS,
};
use crate::steps::Step as Call;
use crate::tools::InputModule;

/// What one recording holds while it runs.
struct Active {
    id: u64,
    session: Option<Box<dyn RecordSession>>,
    started: Instant,
    started_ms: u128,
    deadline: Instant,
    allowed: Vec<String>,
    kept: Vec<Captured>,
    events: usize,
    other_apps: usize,
    unknown: usize,
    lost: usize,
    /// Why listening ended before `desktop_record stop`, if it did.
    ended: Option<&'static str>,
}

impl Active {
    /// Read what the session has and decide what is kept. Events for other
    /// applications are counted here and go no further.
    fn pump(&mut self) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let mut over = false;
        for mut c in session.drain() {
            self.events += 1;
            scrub(&mut c);
            match admit(&c, &self.allowed) {
                Admit::Keep if self.kept.len() < MAX_RAW_EVENTS => self.kept.push(c),
                Admit::Keep => over = true,
                Admit::OtherApp => self.other_apps += 1,
                Admit::Unknown => self.unknown += 1,
            }
        }
        self.lost = session.lost();
        if over {
            self.ended = Some("the event limit was reached");
        } else if Instant::now() >= self.deadline {
            self.ended = Some("the time limit was reached");
        }
        if self.ended.is_some() {
            self.session = None;
        }
    }

    fn listening(&self) -> bool {
        self.session.is_some()
    }
}

/// The recorder's wiring, held by the input module.
#[derive(Default)]
pub struct RecordSupport {
    backend: Option<Arc<dyn RecordBackend>>,
    flows: Option<FlowStore>,
    allowed_apps: Vec<String>,
    marker: Option<PathBuf>,
    state: Arc<Mutex<Option<Active>>>,
    next_id: std::sync::atomic::AtomicU64,
}

impl RecordSupport {
    pub fn new(backend: Option<Arc<dyn RecordBackend>>, allowed_apps: Vec<String>) -> Self {
        RecordSupport {
            backend,
            allowed_apps,
            ..RecordSupport::default()
        }
    }

    pub fn with_flows(mut self, store: FlowStore) -> Self {
        self.flows = Some(store);
        self
    }

    pub fn with_marker(mut self, path: PathBuf) -> Self {
        self.marker = Some(path);
        self
    }

    pub(crate) fn flows(&self) -> Option<&FlowStore> {
        self.flows.as_ref()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Active>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn write_marker(&self, started_ms: u128, expires_ms: u128) {
        let Some(path) = &self.marker else { return };
        let m = ActiveMarker {
            pid: std::process::id(),
            started_ms,
            expires_ms,
            apps: self.allowed_apps.clone(),
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string(&m) {
            let _ = std::fs::write(path, json);
        }
    }

    fn clear_marker(&self) {
        if let Some(path) = &self.marker {
            let _ = std::fs::remove_file(path);
        }
    }

    /// Stop listening when the clock says so, even if nobody asks. The kept
    /// events stay until `stop` collects them.
    fn spawn_expiry(&self, id: u64, after: std::time::Duration) {
        let state = self.state.clone();
        let marker = self.marker.clone();
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(a) = guard.as_mut().filter(|a| a.id == id && a.listening()) {
                a.pump();
                a.session = None;
                a.ended.get_or_insert("the time limit was reached");
                if let Some(path) = &marker {
                    let _ = std::fs::remove_file(path);
                }
            }
        });
    }
}

/// The `doctor` line for a marker file: whether a recording is running now.
/// `None` when there is no marker. A marker whose process is gone or whose time
/// has passed is reported as stale rather than as a recording.
pub fn describe_marker(text: &str, now_ms: u128, pid_alive: &dyn Fn(u32) -> bool) -> String {
    match serde_json::from_str::<ActiveMarker>(text) {
        Err(_) => "a recording marker file exists but cannot be read; delete it".to_string(),
        Ok(m) if !pid_alive(m.pid) => format!(
            "stale marker: the server (pid {}) that was recording has exited; delete the marker file",
            m.pid
        ),
        Ok(m) if now_ms >= m.expires_ms => format!(
            "stale marker: pid {} stopped listening at its time limit",
            m.pid
        ),
        Ok(m) => format!(
            "RECORDING ACTIVE: pid {} is listening to keyboard and mouse in {} for another {} s",
            m.pid,
            if m.apps.is_empty() {
                "no application (nothing is kept)".to_string()
            } else {
                m.apps.join(", ")
            },
            (m.expires_ms - now_ms) / 1000
        ),
    }
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn flow_err(tool: &str, e: FlowError) -> Envelope {
    match e {
        FlowError::Invalid(m) => Envelope::fail(tool, ErrorCode::InvalidArgs, m),
        FlowError::Io(m) => Envelope::fail(tool, ErrorCode::ActionFailed, m),
    }
}

fn counts(a: &Active) -> Value {
    json!({
        "events_seen": a.events,
        "events_kept": a.kept.len(),
        "dropped_other_apps": a.other_apps,
        "dropped_unlocated": a.unknown,
        "lost_by_backend": a.lost,
    })
}

impl InputModule {
    /// `desktop_record`: start, stop or look at a recording.
    pub(crate) async fn desktop_record(&self, args: &Value) -> Envelope {
        let tool = "desktop_record";
        let rec = &self.rec;
        match args
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("status")
        {
            "start" => {
                let Some(backend) = &rec.backend else {
                    return Envelope::fail_with(
                        tool,
                        ErrorCode::UnsupportedOs,
                        "this build has no way to watch the keyboard and mouse",
                        "record on macOS; elsewhere write the flow with desktop_flow save",
                    );
                };
                if rec.allowed_apps.is_empty() {
                    return Envelope::fail_with(
                        tool,
                        ErrorCode::PolicyDenied,
                        "policy.allowed_apps is empty, so a recording would keep nothing",
                        "list the applications that may be recorded in policy.allowed_apps",
                    );
                }
                if rec.lock().as_ref().is_some_and(|a| a.listening()) {
                    return Envelope::fail_with(
                        tool,
                        ErrorCode::ActionFailed,
                        "a recording is already running",
                        "desktop_record stop (with a name to save it) before starting another",
                    );
                }
                let secs = match args.get("max_seconds") {
                    None | Some(Value::Null) => DEFAULT_RECORD_MS / 1000,
                    Some(v) => match mcp_types::args::as_u64(v) {
                        Some(n) if n > 0 => n.min(MAX_RECORD_MS / 1000),
                        _ => {
                            return Envelope::fail(
                                tool,
                                ErrorCode::InvalidArgs,
                                "'max_seconds' must be a positive integer",
                            )
                        }
                    },
                };
                let session = match backend.start_recording() {
                    Ok(s) => s,
                    Err(RecordError::PermissionDenied(m)) => {
                        return Envelope::fail_with(
                            tool,
                            ErrorCode::PermDenied,
                            m,
                            "grant it, restart the app that runs agentctl, and start again; \
                             nothing was recorded",
                        )
                    }
                    Err(RecordError::Unsupported(m)) => {
                        return Envelope::fail_with(
                            tool,
                            ErrorCode::UnsupportedOs,
                            m,
                            "write the flow with desktop_flow save, or record on macOS",
                        )
                    }
                    Err(RecordError::Failed(m)) => {
                        return Envelope::fail(tool, ErrorCode::ActionFailed, m)
                    }
                };
                let id = rec
                    .next_id
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    + 1;
                let started_ms = now_ms();
                let expires_ms = started_ms + (secs as u128) * 1000;
                *rec.lock() = Some(Active {
                    id,
                    session: Some(session),
                    started: Instant::now(),
                    started_ms,
                    deadline: Instant::now() + std::time::Duration::from_secs(secs),
                    allowed: rec.allowed_apps.clone(),
                    kept: Vec::new(),
                    events: 0,
                    other_apps: 0,
                    unknown: 0,
                    lost: 0,
                    ended: None,
                });
                rec.write_marker(started_ms, expires_ms);
                rec.spawn_expiry(id, std::time::Duration::from_secs(secs));
                Envelope::ok(
                    tool,
                    json!({
                        "recording": true,
                        "id": id,
                        "max_seconds": secs,
                        "apps": rec.allowed_apps,
                        "step_limit": MAX_RECORD_STEPS,
                        "note": "listening to the keyboard and mouse now, in the listed applications only; \
                                 text typed into secure fields is never kept. Demonstrate the task, then \
                                 call desktop_record stop with a name",
                    }),
                )
            }
            "status" => {
                let mut guard = rec.lock();
                let Some(a) = guard.as_mut() else {
                    return Envelope::ok(tool, json!({ "recording": false }));
                };
                a.pump();
                let mut data = counts(a);
                data["recording"] = json!(a.listening());
                data["elapsed_ms"] = json!(a.started.elapsed().as_millis() as u64);
                data["steps_so_far"] = json!(reduce(&a.kept).len());
                if let Some(why) = a.ended {
                    data["ended"] = json!(why);
                    data["note"] =
                        json!("listening has stopped; desktop_record stop collects what was kept");
                }
                if !a.listening() {
                    rec.clear_marker();
                }
                Envelope::ok(tool, data)
            }
            "stop" => {
                let taken = {
                    let mut guard = rec.lock();
                    if let Some(a) = guard.as_mut() {
                        a.pump();
                    }
                    guard.take()
                };
                let Some(mut a) = taken else {
                    return Envelope::fail_with(
                        tool,
                        ErrorCode::NotFound,
                        "no recording is running",
                        "desktop_record start first",
                    );
                };
                // Dropping the session is what stops listening.
                a.session = None;
                rec.clear_marker();
                let mut steps = reduce(&a.kept);
                let truncated = steps.len() > MAX_RECORD_STEPS;
                steps.truncate(MAX_RECORD_STEPS);
                let mut data = counts(&a);
                data["recording"] = json!(false);
                data["elapsed_ms"] = json!(a.started.elapsed().as_millis() as u64);
                data["started_ms"] = json!(a.started_ms as u64);
                data["count"] = json!(steps.len());
                if truncated {
                    data["truncated"] =
                        json!(format!("only the first {MAX_RECORD_STEPS} steps are kept"));
                }
                if let Some(why) = a.ended {
                    data["ended_early"] = json!(why);
                }
                let secrets = secret_refs(&steps);
                if !secrets.is_empty() {
                    data["secrets"] = json!(secrets);
                }
                match args.get("name").and_then(Value::as_str) {
                    Some(name) => {
                        let Some(store) = rec.flows() else {
                            return Envelope::fail_with(
                                tool,
                                ErrorCode::UnsupportedOs,
                                "desktop_record cannot save: no flow store is configured",
                                "run agentctl with a state directory",
                            );
                        };
                        match store.save(name, steps, now_ms()) {
                            Ok(f) => {
                                data["saved"] = json!(true);
                                data["name"] = json!(f.name);
                                data["flow"] = json!(f);
                                Envelope::ok(tool, data)
                            }
                            Err(e) => flow_err(tool, e),
                        }
                    }
                    None => {
                        data["steps"] = json!(steps);
                        Envelope::ok(tool, data)
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

    /// `desktop_flow`: keep, read and run recorded flows.
    pub(crate) async fn desktop_flow(&self, args: &Value, ctx: &CallCtx) -> Envelope {
        let tool = "desktop_flow";
        let Some(store) = self.rec.flows() else {
            return Envelope::fail_with(
                tool,
                ErrorCode::UnsupportedOs,
                "no flow store is configured",
                "run agentctl with a state directory",
            );
        };
        let name = args.get("name").and_then(Value::as_str);
        match args.get("action").and_then(Value::as_str).unwrap_or("list") {
            "list" => match store.list() {
                Ok(fs) => Envelope::ok(
                    tool,
                    json!({
                        "flows": fs.iter().map(|f| json!({
                            "name": f.name,
                            "steps": f.steps.len(),
                            "apps": apps_of(f),
                            "secrets": secret_refs(&f.steps),
                            "updated_ms": f.updated_ms as u64,
                        })).collect::<Vec<_>>(),
                        "count": fs.len(),
                    }),
                ),
                Err(e) => flow_err(tool, e),
            },
            "get" => match name.map(|n| store.get(n)) {
                None => Envelope::fail(tool, ErrorCode::InvalidArgs, "'get' needs a 'name'"),
                Some(Err(e)) => flow_err(tool, e),
                Some(Ok(None)) => not_found(tool, name.unwrap_or_default()),
                Some(Ok(Some(f))) => Envelope::ok(tool, json!({ "flow": f })),
            },
            "delete" => match name.map(|n| store.delete(n)) {
                None => Envelope::fail(tool, ErrorCode::InvalidArgs, "'delete' needs a 'name'"),
                Some(Err(e)) => flow_err(tool, e),
                Some(Ok(false)) => not_found(tool, name.unwrap_or_default()),
                Some(Ok(true)) => Envelope::ok(tool, json!({ "deleted": true })),
            },
            "save" => {
                let (Some(name), Some(raw)) = (name, args.get("steps")) else {
                    return Envelope::fail(
                        tool,
                        ErrorCode::InvalidArgs,
                        "'save' needs a 'name' and 'steps'",
                    );
                };
                let steps: Vec<Step> = match serde_json::from_value(raw.clone()) {
                    Ok(s) => s,
                    Err(e) => {
                        return Envelope::fail_with(
                            tool,
                            ErrorCode::InvalidArgs,
                            format!("steps are not valid: {e}"),
                            "a step is {op: click|type|shortcut|set, locator?, text?, combo?, secret_ref?}; \
                             desktop_flow get shows a recorded one",
                        )
                    }
                };
                match store.save(name, steps, now_ms()) {
                    Ok(f) => Envelope::ok(
                        tool,
                        json!({ "saved": true, "name": f.name, "steps": f.steps.len() }),
                    ),
                    Err(e) => flow_err(tool, e),
                }
            }
            "run" => {
                let Some(name) = name else {
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, "'run' needs a 'name'");
                };
                let flow = match store.get(name) {
                    Ok(Some(f)) => f,
                    Ok(None) => return not_found(tool, name),
                    Err(e) => return flow_err(tool, e),
                };
                let secrets: BTreeMap<String, String> = args
                    .get("secrets")
                    .and_then(Value::as_object)
                    .map(|m| {
                        m.iter()
                            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                            .collect()
                    })
                    .unwrap_or_default();
                self.run_flow(tool, &flow, &secrets, ctx).await
            }
            other => Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                format!("unknown flow action '{other}'; use list, get, save, delete or run"),
            ),
        }
    }

    /// Snapshot `app` without pinning it, install it so its refs resolve, and
    /// return the elements a locator can match.
    async fn flow_candidates(
        &self,
        app: &str,
    ) -> Result<Vec<crate::record::Candidate>, (ErrorCode, String)> {
        let Some(observer) = &self.observer else {
            return Err((
                ErrorCode::UnsupportedOs,
                "no accessibility reader is wired, so a locator cannot be resolved".into(),
            ));
        };
        let raw = observer
            .snapshot(&SnapshotRequest {
                app: Some(app.to_string()),
                without_pin: true,
                ..SnapshotRequest::default()
            })
            .await
            .map_err(|e| match e {
                BackendError::PermissionDenied(m) => (ErrorCode::PermDenied, m),
                BackendError::NotFound(m) => (ErrorCode::NotFound, m),
                BackendError::Unsupported(m) => (ErrorCode::UnsupportedOs, m),
                BackendError::Failed(m) => (ErrorCode::ActionFailed, m),
            })?;
        let mut arena = self.arena.lock().unwrap_or_else(|e| e.into_inner());
        let sid = arena.next_id();
        let f = flatten(
            &raw.root,
            raw.app.as_deref(),
            raw.window.as_deref(),
            &sid,
            &FlattenConfig {
                max_chars: usize::MAX,
                terminal_app: raw.terminal_app,
                ..FlattenConfig::default()
            },
        );
        let refs = f
            .snapshot
            .elements
            .iter()
            .filter_map(|(r, i)| i.node_id.map(|id| (id, r.clone())))
            .collect();
        let cands = candidates_of(&raw.root, &refs);
        arena.install(f.snapshot);
        Ok(cands)
    }

    /// Run a flow, step by step, through the gate.
    async fn run_flow(
        &self,
        tool: &str,
        flow: &DesktopFlow,
        secrets: &BTreeMap<String, String>,
        ctx: &CallCtx,
    ) -> Envelope {
        let Some(dispatch) = self.relay.as_ref().and_then(|r| r.get()) else {
            return Envelope::fail_with(
                tool,
                ErrorCode::UnsupportedOs,
                "desktop_flow is not connected to this server's gate, so it will not run steps",
                "call the tools one at a time",
            );
        };
        // Everything that can be checked before anything runs is, so a run
        // never gets half way and then discovers it could not finish.
        if let Err(m) = validate_secrets(&flow.steps) {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, m);
        }
        let missing = missing_secrets(&flow.steps, secrets);
        if !missing.is_empty() {
            return Envelope::fail_with(
                tool,
                ErrorCode::InvalidArgs,
                format!("the flow needs secrets that were not supplied: {}", missing.join(", ")),
                format!(
                    "pass {{\"secrets\": {{{}}}}} when running the flow; they are used in memory only",
                    missing
                        .iter()
                        .map(|m| format!("\"{m}\": \"...\""))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
        }
        if self.policy.foreground.is_never() {
            for (i, s) in flow.steps.iter().enumerate() {
                if let Some((t, a)) = planned_call(s, "@e0", secrets) {
                    if let Some(r) = crate::foreground::tool_refusal(&*self.backend, t, &a) {
                        let r = mcp_types::Refusal {
                            what: format!("desktop_flow step {i} ({})", r.what),
                            ..r
                        };
                        return r.envelope(tool);
                    }
                }
            }
        }

        let since = self.backend.takeover_generation();
        let started = Instant::now();
        let mut rows: Vec<Value> = Vec::new();
        let mut failed: Option<(usize, ErrorCode, String, String)> = None;
        let mut ran = 0;

        for (i, step) in flow.steps.iter().enumerate() {
            let stop = if self.halt.as_ref().is_some_and(|h| h()) {
                Some("the kill switch is engaged")
            } else if self.backend.takeover_generation() != since {
                Some("a human took over the pointer")
            } else if ctx.cancel.is_cancelled() {
                Some("the call was cancelled")
            } else {
                None
            };
            if let Some(why) = stop {
                rows.push(json!({ "i": i, "op": step.op, "ok": false, "skipped": true,
                    "error": { "code": ErrorCode::Timeout, "message": why } }));
                failed = Some((
                    i,
                    ErrorCode::Timeout,
                    format!("stopped before step {i}: {why}"),
                    format!("steps from {i} on did not run"),
                ));
                break;
            }
            match self.flow_step(i, step, secrets, &*dispatch, ctx).await {
                Ok(row) => {
                    ran += 1;
                    rows.push(row);
                    // The next step reads the tree again; give the app a moment
                    // to finish reacting to this one.
                    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                }
                Err((row, code, msg, hint)) => {
                    ran += 1;
                    rows.push(row);
                    failed = Some((i, code, msg, hint));
                    break;
                }
            }
        }

        let mut data = json!({
            "name": flow.name,
            "ran": ran,
            "total": flow.steps.len(),
            "elapsed_ms": started.elapsed().as_millis() as u64,
            "steps": rows,
        });
        match failed {
            None => Envelope::ok(tool, data),
            Some((i, code, msg, hint)) => {
                data["failed_at"] = json!(i);
                Envelope::fail_with_data(tool, code, msg, hint, data)
            }
        }
    }

    /// One step: find what it names, then make the call it stands for.
    #[allow(clippy::type_complexity)]
    async fn flow_step(
        &self,
        i: usize,
        step: &Step,
        secrets: &BTreeMap<String, String>,
        dispatch: &dyn crate::steps::StepDispatch,
        ctx: &CallCtx,
    ) -> Result<Value, (Value, ErrorCode, String, String)> {
        let op = step.op;
        let fail = |row: Value, code: ErrorCode, msg: String, hint: &str| {
            Err((
                row,
                code,
                format!("step {i} ({op:?}) failed: {msg}").to_lowercase(),
                hint.to_string(),
            ))
        };
        let mut row = json!({ "i": i, "op": op });

        // A chord needs no element.
        let (reff, matched) = match (&step.locator, op) {
            (_, Op::Shortcut) => (None, None),
            (None, _) => {
                return fail(
                    json!({ "i": i, "op": op, "ok": false }),
                    ErrorCode::InvalidArgs,
                    "the step has no locator".into(),
                    "record it again, or add a locator",
                )
            }
            (Some(loc), _) => {
                let cands = match self.flow_candidates(&loc.app).await {
                    Ok(c) => c,
                    Err((code, m)) => {
                        row["ok"] = json!(false);
                        return fail(
                            row,
                            code,
                            m,
                            "check that the application is running and Accessibility is granted",
                        );
                    }
                };
                match resolve(loc, &cands) {
                    Resolution::Found { index, kind } if index != usize::MAX => {
                        (Some(cands[index].reff.clone()), Some(kind))
                    }
                    Resolution::Found { .. } => (None, Some(MatchKind::Coordinate)),
                    Resolution::Ambiguous(idx) => {
                        row["ok"] = json!(false);
                        row["candidates"] = json!(idx
                            .iter()
                            .map(|&k| cands[k].reff.clone())
                            .collect::<Vec<_>>());
                        return fail(
                            row,
                            ErrorCode::ActionFailed,
                            format!(
                                "{} elements fit {} {:?} equally well ({}), and acting on one would be a guess",
                                idx.len(),
                                loc.role,
                                loc.title.clone().unwrap_or_default(),
                                idx.iter().map(|&k| cands[k].reff.as_str()).collect::<Vec<_>>().join(", ")
                            ),
                            "re-record the step with a more specific target, or add a title to its locator",
                        );
                    }
                    Resolution::NotFound => {
                        row["ok"] = json!(false);
                        return fail(
                            row,
                            ErrorCode::NotFound,
                            format!(
                                "no {} {} was found in {}",
                                loc.role,
                                loc.title.as_ref().map(|t| format!("titled {t:?}")).unwrap_or_default(),
                                loc.app
                            ),
                            "the interface differs from the recording: look with observe or get_ui_tree, then fix or re-record the step",
                        );
                    }
                }
            }
        };
        if let Some(m) = matched {
            row["matched"] = json!(m);
        }

        // The calls this step stands for.
        let mut calls: Vec<Call> = Vec::new();
        match (op, &reff) {
            (Op::Click, Some(r)) => {
                if step.button.as_deref() == Some("middle") {
                    row["ok"] = json!(false);
                    return fail(
                        row,
                        ErrorCode::UnsupportedOs,
                        "a middle click has no accessibility form".into(),
                        "replay it by hand",
                    );
                }
                let role = step
                    .locator
                    .as_ref()
                    .map(|l| l.role.as_str())
                    .unwrap_or_default();
                let action = click_action(role, step.button.as_deref(), step.clicks);
                calls.push(Call {
                    tool: "ui_action".into(),
                    args: json!({ "ref": r, "action": action }),
                });
            }
            (Op::Click, None) => {
                let Some(h) = step.locator.as_ref().and_then(|l| l.hint) else {
                    row["ok"] = json!(false);
                    return fail(
                        row,
                        ErrorCode::NotFound,
                        "nothing matched and no coordinate was recorded".into(),
                        "re-record the step",
                    );
                };
                let kind = match (step.button.as_deref(), step.clicks) {
                    (Some("right"), _) => "right_click",
                    (_, Some(2)) => "double",
                    (_, Some(n)) if n >= 3 => "triple",
                    _ => "click",
                };
                calls.push(Call {
                    tool: "mouse_action".into(),
                    args: json!({ "type": kind, "x": h.x, "y": h.y }),
                });
            }
            (Op::Type | Op::Set, None) => {
                row["ok"] = json!(false);
                return fail(
                    row,
                    ErrorCode::NotFound,
                    "the field could not be found, and typing without it would go to whatever has the focus".into(),
                    "look with observe or get_ui_tree, then fix or re-record the step",
                );
            }
            (Op::Type, Some(r)) | (Op::Set, Some(r)) => {
                let secret = step.secret_ref.is_some();
                let text = match &step.secret_ref {
                    Some(name) => secrets.get(name).cloned().unwrap_or_default(),
                    None => step.text.clone().unwrap_or_default(),
                };
                let tool_name = if op == Op::Set {
                    "set_value"
                } else {
                    "keyboard_type"
                };
                let mut a = json!({ "ref": r, "text": text });
                if secret {
                    a["secret"] = json!(true);
                }
                calls.push(Call {
                    tool: tool_name.into(),
                    args: a,
                });
            }
            (Op::Shortcut, _) => {
                let Some(c) = &step.combo else {
                    row["ok"] = json!(false);
                    return fail(
                        row,
                        ErrorCode::InvalidArgs,
                        "the step has no combo".into(),
                        "fix the step",
                    );
                };
                calls.push(Call {
                    tool: "keyboard_shortcut".into(),
                    args: json!({ "combo": c }),
                });
            }
        }

        for call in calls {
            row["tool"] = json!(call.tool);
            if let Some(r) = call.args.get("ref") {
                row["ref"] = r.clone();
            }
            let env = crate::tools::watch_call(dispatch, &call, ctx).await;
            if let Some(Value::Object(d)) = &env.data {
                // Which route acted: ax, hid or pid_event.
                if let Some(route) = d.get("input") {
                    row["route"] = route.clone();
                }
            }
            if !env.ok {
                row["ok"] = json!(false);
                let (code, msg, hint) = env
                    .error
                    .map_or((ErrorCode::ActionFailed, String::new(), None), |e| {
                        (e.code, e.message, e.suggestion)
                    });
                row["error"] = json!({ "code": code, "message": msg });
                let h = hint.unwrap_or_else(|| {
                    format!("steps before {i} ran; fix step {i} and run the flow again, or finish by hand")
                });
                return Err((
                    row,
                    code,
                    format!("step {i} ({}) failed: {msg}", call.tool),
                    h,
                ));
            }
        }
        row["ok"] = json!(true);
        Ok(row)
    }
}

/// The call a step will make, with a placeholder ref, for checks that run
/// before the first step. A click is planned as the accessibility press it
/// normally is; the coordinate fallback is judged if and when it happens.
fn planned_call(
    step: &Step,
    placeholder: &str,
    secrets: &BTreeMap<String, String>,
) -> Option<(&'static str, Value)> {
    match step.op {
        Op::Click => None,
        Op::Shortcut => Some((
            "keyboard_shortcut",
            json!({ "combo": step.combo.clone().unwrap_or_default() }),
        )),
        Op::Type => Some((
            "keyboard_type",
            json!({ "ref": placeholder, "text": step.text.clone().or_else(|| step.secret_ref.as_ref().and_then(|r| secrets.get(r).cloned())).unwrap_or_default() }),
        )),
        Op::Set => Some(("set_value", json!({ "ref": placeholder, "text": "" }))),
    }
}

fn apps_of(f: &DesktopFlow) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    for s in &f.steps {
        if let Some(l) = &s.locator {
            if !v.contains(&l.app) {
                v.push(l.app.clone());
            }
        }
    }
    v
}

fn not_found(tool: &str, name: &str) -> Envelope {
    Envelope::fail_with(
        tool,
        ErrorCode::NotFound,
        format!("no desktop flow named '{name}'"),
        "desktop_flow list shows the saved ones",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_doctor_line_says_active_stale_or_unreadable() {
        let alive = |p: u32| p == 7;
        let m = r#"{"pid":7,"started_ms":1000,"expires_ms":61000,"apps":["TextEdit","Finder"]}"#;
        let active = describe_marker(m, 11_000, &alive);
        assert!(active.starts_with("RECORDING ACTIVE"), "{active}");
        assert!(
            active.contains("TextEdit, Finder") && active.contains("50 s"),
            "{active}"
        );
        assert!(describe_marker(m, 70_000, &alive).contains("time limit"));
        let dead = m.replace("\"pid\":7", "\"pid\":8");
        assert!(describe_marker(&dead, 11_000, &alive).contains("has exited"));
        assert!(describe_marker("{", 0, &alive).contains("cannot be read"));
        let none = r#"{"pid":7,"started_ms":0,"expires_ms":9000,"apps":[]}"#;
        assert!(describe_marker(none, 1, &alive).contains("nothing is kept"));
    }

    #[test]
    fn a_planned_call_names_the_tool_the_gate_will_see() {
        let secrets: BTreeMap<String, String> = [("pw".to_string(), "x".to_string())].into();
        let mut s = Step {
            op: Op::Shortcut,
            locator: None,
            text: None,
            combo: Some("cmd+s".into()),
            secret_ref: None,
            button: None,
            clicks: None,
            menu: None,
            at_ms: 0,
        };
        assert_eq!(
            planned_call(&s, "@e0", &secrets).unwrap().0,
            "keyboard_shortcut"
        );
        s.op = Op::Type;
        s.text = Some("hi".into());
        assert_eq!(
            planned_call(&s, "@e0", &secrets).unwrap().0,
            "keyboard_type"
        );
        s.op = Op::Set;
        assert_eq!(planned_call(&s, "@e0", &secrets).unwrap().0, "set_value");
        s.op = Op::Click;
        assert!(planned_call(&s, "@e0", &secrets).is_none());
    }
}
