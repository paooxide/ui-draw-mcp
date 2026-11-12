//! `foreground = "polite"` against the real server, gate and watcher.
//!
//! The backend is a recorder, as in `input_steps.rs`: it proves when the
//! engine waited, what it borrowed, what it gave back and in what order, and
//! claims nothing about what an OS does with a pointer. That is measured
//! against the real desktop in `live_desktop_input.rs`. Everything else is the
//! real thing: policy, gate, audit, registry, the batch's route back through
//! `bind_input_steps`, and the human-override watcher that `serve` spawns.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use mcp_core::{Registry, Server};
use mcp_input::{
    Activity, AppRef, ClipData, ClipFormat, DeskState, InputBackend, InputError, InputModule,
    InputPolicy, MouseKind, PoliteConfig, Route, ScrollDir, SemanticAction, SetPoint, StepRelay,
};
use mcp_policy::{AuditSink, NoConsent, Policy, PolicyConfig, Redactor};
use mcp_types::{Category, Envelope, ErrorCode, Foreground, ToolModule};
use serde_json::{json, Value};
use test_support::{test_policy, InProcClient};

fn app(pid: i32, name: &str) -> Option<AppRef> {
    Some(AppRef {
        pid,
        name: name.into(),
    })
}

/// Writes down what reaches it. The "desk" is a pair of values the recorder
/// moves when it is clicked at and puts back when asked, so a test can tell a
/// restore that happened from one that did not.
struct Recorder {
    log: Mutex<Vec<String>>,
    /// What `user_idle_ms` answers; `None` is a platform with no idle clock.
    idle: Mutex<Option<u64>>,
    desk: Mutex<DeskState>,
    takeovers: AtomicU64,
    /// Where the pointer is and what the server last set, for the watcher.
    pointer: Mutex<Option<(f64, f64)>>,
    sets: Mutex<Vec<SetPoint>>,
    /// Makes the next click look like a person taking the pointer mid-action.
    takeover_on_click: Mutex<bool>,
    /// Makes the next click fail after it has moved the pointer.
    fail_click: Mutex<bool>,
    policy: OnceLock<Arc<Policy>>,
}

impl Default for Recorder {
    fn default() -> Self {
        Recorder {
            log: Mutex::new(Vec::new()),
            idle: Mutex::new(Some(60_000)),
            desk: Mutex::new(DeskState {
                app: app(10, "Mail"),
                pointer: Some((300.0, 300.0)),
            }),
            takeovers: AtomicU64::new(0),
            pointer: Mutex::new(Some((300.0, 300.0))),
            sets: Mutex::new(Vec::new()),
            takeover_on_click: Mutex::new(false),
            fail_click: Mutex::new(false),
            policy: OnceLock::new(),
        }
    }
}

impl Recorder {
    fn note(&self, what: impl Into<String>) {
        self.log.lock().unwrap().push(what.into());
    }
    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
    fn restores(&self) -> Vec<String> {
        self.log()
            .into_iter()
            .filter(|l| l.starts_with("restore"))
            .collect()
    }
}

#[async_trait]
impl InputBackend for Recorder {
    async fn perform(
        &self,
        node_id: u64,
        action: SemanticAction,
        _option: Option<&str>,
    ) -> Result<Route, InputError> {
        self.note(format!("perform {node_id} {action:?}"));
        Ok(Route::Ax)
    }
    fn route_for(&self, _action: SemanticAction) -> Route {
        Route::Ax
    }
    async fn set_value(&self, node_id: u64, text: &str) -> Result<(), InputError> {
        self.note(format!("set_value {node_id} {text}"));
        Ok(())
    }
    async fn type_text(&self, text: &str) -> Result<(), InputError> {
        self.note(format!("type {text}"));
        Ok(())
    }
    async fn key_combo(&self, combo: &str) -> Result<(), InputError> {
        self.note(format!("key {combo}"));
        Ok(())
    }
    async fn mouse(
        &self,
        kind: MouseKind,
        x: f64,
        y: f64,
        _button: Option<&str>,
        _modifiers: &[String],
    ) -> Result<(), InputError> {
        self.note(format!("mouse {kind:?} {x} {y}"));
        // A click moves the pointer and raises whatever it lands on.
        {
            let mut d = self.desk.lock().unwrap();
            d.pointer = Some((x, y));
            d.app = app(20, "Notes");
        }
        if *self.fail_click.lock().unwrap() {
            return Err(InputError::Failed("the click failed".into()));
        }
        if *self.takeover_on_click.lock().unwrap() {
            self.cancel_pending();
            return Err(InputError::Failed(
                "aborted: a human took over the pointer".into(),
            ));
        }
        Ok(())
    }
    async fn scroll_at(
        &self,
        _x: f64,
        _y: f64,
        _dir: ScrollDir,
        _amount: i32,
        _modifiers: &[String],
    ) -> Result<(), InputError> {
        self.note("scroll");
        Ok(())
    }
    async fn hover(&self, _x: f64, _y: f64) -> Result<(), InputError> {
        self.note("hover");
        Ok(())
    }
    async fn drag(
        &self,
        _from: (f64, f64),
        _route: &[(f64, f64)],
        _modifiers: &[String],
        _hold_ms: u64,
        _since_takeover: u64,
    ) -> Result<(), InputError> {
        self.note("drag");
        Ok(())
    }
    async fn key_down(&self, key: &str) -> Result<(), InputError> {
        self.note(format!("down {key}"));
        Ok(())
    }
    async fn key_up(&self, key: &str) -> Result<(), InputError> {
        self.note(format!("up {key}"));
        Ok(())
    }
    async fn clipboard_read(&self, format: ClipFormat) -> Result<ClipData, InputError> {
        Ok(ClipData { format, data: None })
    }
    fn input_target(&self) -> Option<String> {
        Some("TestApp".into())
    }
    async fn clipboard_write(&self, _format: ClipFormat, _data: &str) -> Result<(), InputError> {
        Ok(())
    }
    fn platform(&self) -> &'static str {
        "recorder"
    }
    async fn user_idle_ms(&self) -> Option<u64> {
        *self.idle.lock().unwrap()
    }
    async fn desk_state(&self) -> DeskState {
        self.desk.lock().unwrap().clone()
    }
    fn restore_desk(&self, saved: &DeskState, pointer: bool) {
        self.note(format!("restore pointer={pointer}"));
        let mut d = self.desk.lock().unwrap();
        d.app = saved.app.clone();
        if pointer {
            d.pointer = saved.pointer;
        }
    }
    async fn pointer_position(&self) -> Result<Option<(f64, f64)>, InputError> {
        Ok(*self.pointer.lock().unwrap())
    }
    fn recent_pointer_sets(&self) -> Vec<SetPoint> {
        self.sets.lock().unwrap().clone()
    }
    fn cancel_pending(&self) {
        self.takeovers.fetch_add(1, Ordering::SeqCst);
        self.note("cancel_pending");
    }
    fn takeover_generation(&self) -> u64 {
        self.takeovers.load(Ordering::SeqCst)
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    client: InProcClient,
    policy: Arc<Policy>,
    rec: Arc<Recorder>,
    activity: Arc<Activity>,
}

fn polite_policy() -> PolicyConfig {
    let mut cfg = test_policy(&[Category::Input]);
    cfg.foreground = Foreground::Polite;
    cfg
}

impl Rig {
    fn new(mut cfg: PolicyConfig) -> Rig {
        let dir = tempfile::tempdir().expect("temp dir");
        cfg.kill_switch_file = dir.path().join("STOP");
        let rec = Arc::new(Recorder::default());
        let activity = Activity::new();
        let relay = StepRelay::new();
        let module: Arc<InputModule> = Arc::new(
            InputModule::new(
                rec.clone(),
                Arc::new(Mutex::new(mcp_a11y::SnapshotArena::new())),
                InputPolicy {
                    autonomous: true,
                    foreground: cfg.foreground,
                    polite: PoliteConfig {
                        idle_ms: 100,
                        max_wait_ms: 300,
                    },
                    ..InputPolicy::default()
                },
            )
            .with_steps(relay.clone())
            .with_activity(activity.clone()),
        );
        let policy = Arc::new(
            Policy::new(cfg, AuditSink::memory(), Redactor::empty())
                .with_consent(Arc::new(NoConsent)),
        );
        rec.policy.set(policy.clone()).ok();
        let mut wiring = agentctl::Wiring::default();
        let as_module: Arc<dyn ToolModule> = module.clone();
        wiring.steps = Some((as_module.clone(), relay));
        agentctl::bind_input_steps(&wiring, &policy, "sess").expect("binds");
        let server = Server::new(
            Registry::build(vec![as_module]).expect("registry"),
            policy.clone(),
            "sess",
        );
        Rig {
            _dir: dir,
            client: InProcClient::new(server),
            policy,
            rec,
            activity,
        }
    }

    async fn call(&self, tool: &str, args: Value) -> Envelope {
        self.client.call(tool, args).await
    }
}

fn click(x: f64, y: f64) -> Value {
    json!({"type": "click", "x": x, "y": y})
}

#[tokio::test]
async fn a_click_borrows_the_desk_and_gives_the_pointer_and_app_back() {
    let rig = Rig::new(polite_policy());
    let env = rig.call("mouse_action", click(500.0, 400.0)).await;
    assert!(env.ok, "{env:?}");
    let polite = &env.data.as_ref().unwrap()["polite"];
    assert_eq!(polite["returned"]["app"], json!(true), "{polite}");
    assert_eq!(polite["returned"]["pointer"], json!(true), "{polite}");
    assert_eq!(polite["user_active"], json!(false));
    let log = rig.rec.log();
    assert!(log[0].starts_with("mouse Click"), "{log:?}");
    assert_eq!(rig.rec.restores(), ["restore pointer=true"], "{log:?}");
    // The desk really is back, not just reported as back.
    let desk = rig.rec.desk.lock().unwrap().clone();
    assert_eq!(desk.pointer, Some((300.0, 300.0)));
    assert_eq!(desk.app, app(10, "Mail"));
}

#[tokio::test]
async fn an_action_waits_while_the_user_is_active_and_then_fails_retryably_having_done_nothing() {
    let rig = Rig::new(polite_policy());
    *rig.rec.idle.lock().unwrap() = Some(0);
    let started = std::time::Instant::now();
    let env = rig.call("mouse_action", click(500.0, 400.0)).await;
    let err = env.error.expect("busy");
    assert_eq!(err.code, ErrorCode::Timeout);
    assert!(err.message.contains("user is active"), "{}", err.message);
    assert!(err.suggestion.unwrap().contains("retryable"));
    assert!(
        started.elapsed() >= Duration::from_millis(250),
        "it must have waited out polite_max_wait_ms (300) before giving up"
    );
    assert!(
        rig.rec.log().is_empty(),
        "nothing was done and nothing restored"
    );
}

#[tokio::test]
async fn the_wait_ends_as_soon_as_the_user_goes_idle() {
    let rig = Rig::new(polite_policy());
    *rig.rec.idle.lock().unwrap() = Some(0);
    let rec = rig.rec.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(120)).await;
        *rec.idle.lock().unwrap() = Some(5_000);
    });
    let env = rig.call("mouse_action", click(10.0, 10.0)).await;
    assert!(env.ok, "{env:?}");
    let waited = env.data.unwrap()["polite"]["waited_ms"].as_u64().unwrap();
    assert!((100..300).contains(&waited), "waited {waited} ms");
}

#[tokio::test]
async fn a_platform_that_cannot_tell_idleness_is_refused_not_guessed() {
    let rig = Rig::new(polite_policy());
    *rig.rec.idle.lock().unwrap() = None;
    let env = rig
        .call("scroll", json!({"direction": "down", "x": 1, "y": 1}))
        .await;
    assert_eq!(env.error.expect("refused").code, ErrorCode::UnsupportedOs);
    assert!(rig.rec.log().is_empty());
}

#[tokio::test]
async fn an_accessibility_action_needs_no_idle_wait_and_restores_nothing() {
    let rig = Rig::new(polite_policy());
    *rig.rec.idle.lock().unwrap() = Some(0);
    // No snapshot is installed, so the ref cannot resolve: the point is only
    // that the call was never held up behind the idle wait, nor restored.
    let started = std::time::Instant::now();
    let _ = rig
        .call("set_value", json!({"ref": "@e1", "text": "x"}))
        .await;
    assert!(started.elapsed() < Duration::from_millis(250));
    assert!(rig.rec.restores().is_empty());
}

#[tokio::test]
async fn the_desk_is_given_back_when_the_action_fails_part_way() {
    let rig = Rig::new(polite_policy());
    // The click moves the pointer and raises another app, then fails.
    *rig.rec.fail_click.lock().unwrap() = true;
    let env = rig.call("mouse_action", click(500.0, 400.0)).await;
    assert!(!env.ok);
    assert_eq!(rig.rec.restores(), ["restore pointer=true"]);
    let desk = rig.rec.desk.lock().unwrap().clone();
    assert_eq!(desk.pointer, Some((300.0, 300.0)));
    assert_eq!(desk.app, app(10, "Mail"));
    assert_eq!(
        env.data.unwrap()["polite"]["returned"]["pointer"],
        json!(true),
        "a failed call still reports the desk it gave back"
    );
}

/// An action that changed nothing needs nothing put back, and none is posted:
/// a pointer warp for no reason would itself be the rudeness.
#[tokio::test]
async fn nothing_is_restored_when_the_action_moved_nothing() {
    let rig = Rig::new(polite_policy());
    let env = rig.call("mouse_action", json!({"type": "click"})).await;
    assert!(
        !env.ok,
        "a click with no coordinates fails on its arguments"
    );
    assert!(rig.rec.restores().is_empty(), "{:?}", rig.rec.log());
}

#[tokio::test]
async fn a_loan_that_is_dropped_still_puts_the_desk_back() {
    let rec = Arc::new(Recorder::default());
    let loan = mcp_input::polite::Loan::borrow(
        rec.clone(),
        PoliteConfig {
            idle_ms: 0,
            max_wait_ms: 100,
        },
        || None,
    )
    .await
    .expect("borrows");
    rec.desk.lock().unwrap().app = app(20, "Notes");
    drop(loan);
    assert_eq!(rec.restores(), ["restore pointer=true"]);
    assert_eq!(rec.desk.lock().unwrap().app, app(10, "Mail"));
}

#[tokio::test]
async fn a_batch_borrows_once_and_gives_back_once_at_the_end() {
    let rig = Rig::new(polite_policy());
    let env = rig
        .call(
            "input_steps",
            json!({"steps": [
                {"tool": "mouse_action", "args": click(500.0, 400.0)},
                {"tool": "keyboard_type", "args": {"text": "hi"}},
                {"tool": "mouse_action", "args": click(600.0, 450.0)},
            ]}),
        )
        .await;
    assert!(env.ok, "{env:?}");
    let log = rig.rec.log();
    assert_eq!(
        rig.rec.restores(),
        ["restore pointer=true"],
        "one restore for three steps: {log:?}"
    );
    assert_eq!(log.last().unwrap(), "restore pointer=true", "{log:?}");
    assert_eq!(
        env.data.unwrap()["polite"]["returned"]["pointer"],
        json!(true)
    );
}

#[tokio::test]
async fn a_held_key_is_refused_alone_and_in_a_batch_before_anything_runs() {
    let rig = Rig::new(polite_policy());
    let env = rig
        .call(
            "keyboard_shortcut",
            json!({"combo": "shift", "action": "down"}),
        )
        .await;
    assert_eq!(env.error.expect("refused").code, ErrorCode::PolicyDenied);
    let env = rig
        .call(
            "input_steps",
            json!({"steps": [
                {"tool": "mouse_action", "args": click(1.0, 1.0)},
                {"tool": "keyboard_shortcut", "args": {"combo": "shift", "action": "down"}},
            ]}),
        )
        .await;
    assert_eq!(env.error.expect("refused").code, ErrorCode::PolicyDenied);
    assert!(
        rig.rec.log().is_empty(),
        "the click before the held key must not have run: {:?}",
        rig.rec.log()
    );
}

#[tokio::test]
async fn a_person_taking_the_pointer_mid_action_ends_it_retryably_and_leaves_them_the_pointer() {
    let rig = Rig::new(polite_policy());
    *rig.rec.takeover_on_click.lock().unwrap() = true;
    let env = rig.call("mouse_action", click(500.0, 400.0)).await;
    let err = env.error.expect("stopped");
    assert_eq!(err.code, ErrorCode::Timeout);
    assert!(
        err.message.starts_with("the user is active"),
        "{}",
        err.message
    );
    assert!(err.suggestion.unwrap().contains("retryable"));
    assert_eq!(
        rig.rec.restores(),
        ["restore pointer=false"],
        "the app is put back but the pointer is not warped out of their hand"
    );
    assert!(!rig.policy.kill_switch_tripped());
}

/// The kill-switch change, exercised through the real watcher: the same
/// pointer divergence trips the switch under `allow` and does not under
/// `polite`, where it abandons the action in flight instead.
async fn pointer_taken_by_a_person(foreground: Foreground) -> (bool, Vec<String>) {
    let mut cfg = test_policy(&[Category::Input]);
    cfg.foreground = foreground;
    let rig = Rig::new(cfg);
    let input: Arc<dyn InputBackend> = rig.rec.clone();
    agentctl::override_watch::spawn(
        input,
        rig.activity.clone(),
        mcp_input::OverrideConfig {
            enabled: true,
            threshold_px: 12.0,
            grace_ms: 1_500,
            poll_ms: 10,
            confirm_samples: 2,
            set_window_ms: 1_000,
        },
        rig.policy.clone(),
        None,
        "sess".into(),
        foreground.is_polite(),
    );
    // The server is driving, and has put the pointer at (100, 100)...
    let _driving = rig.activity.begin();
    let now = mcp_policy::now_ms() as u64;
    rig.rec.sets.lock().unwrap().push(SetPoint {
        x: 100.0,
        y: 100.0,
        at_ms: now,
    });
    *rig.rec.pointer.lock().unwrap() = Some((100.0, 100.0));
    tokio::time::sleep(Duration::from_millis(80)).await;
    // ...and then a hand moves it somewhere the server never put it.
    *rig.rec.pointer.lock().unwrap() = Some((700.0, 650.0));
    tokio::time::sleep(Duration::from_millis(300)).await;
    (rig.policy.kill_switch_tripped(), rig.rec.log())
}

#[tokio::test]
async fn the_watcher_trips_the_kill_switch_under_allow() {
    let (tripped, log) = pointer_taken_by_a_person(Foreground::Allow).await;
    assert!(tripped, "{log:?}");
}

#[tokio::test]
async fn the_watcher_abandons_the_action_but_does_not_trip_the_kill_switch_under_polite() {
    let (tripped, log) = pointer_taken_by_a_person(Foreground::Polite).await;
    assert!(!tripped, "polite must not trip the kill switch: {log:?}");
    assert!(
        log.iter().any(|l| l == "cancel_pending"),
        "the in-flight action must still be abandoned: {log:?}"
    );
}

/// The STOP file is the hard stop in every mode: a call is refused with the
/// file present under polite as under allow.
#[tokio::test]
async fn the_stop_file_still_stops_everything_under_polite() {
    let rig = Rig::new(polite_policy());
    rig.policy.trip_kill_switch("sess", "test");
    let env = rig.call("mouse_action", click(1.0, 1.0)).await;
    assert!(!env.ok);
    assert!(rig.rec.log().is_empty());
}
