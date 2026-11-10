//! `input_steps` against the real server: a batch must be no way around a
//! check that the same calls made one at a time would meet.
//!
//! The backend is a recorder, as in the input engine's own tests: it proves
//! what did and did not reach the OS layer (argument plumbing and refusals),
//! and claims nothing about what an OS does. Everything else is real: the
//! policy, the gate, the audit sink, the registry and the batch's route back
//! through `agentctl::bind_input_steps`, the one `build_server_with` uses.

use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use mcp_a11y::{ElementInfo, Snapshot, SnapshotArena};
use mcp_core::{Registry, Server};
use mcp_input::{
    ClipData, ClipFormat, ForegroundInput, InputBackend, InputError, InputModule, InputPolicy,
    MouseKind, Route, ScrollDir, SemanticAction, StepRelay,
};
use mcp_policy::{AuditSink, Mode, NoConsent, Policy, PolicyConfig, Redactor, RoleProfile};
use mcp_types::{Category, Envelope, ErrorCode, Foreground, ToolModule};
use serde_json::{json, Value};
use test_support::{test_policy, InProcClient};

/// Writes down what reaches it, and can trip the kill switch when asked to
/// type a particular string, to stage the switch engaging mid-batch.
#[derive(Default)]
struct Recorder {
    log: Mutex<Vec<String>>,
    policy: OnceLock<Arc<Policy>>,
}

impl Recorder {
    fn note(&self, what: impl Into<String>) {
        self.log.lock().unwrap().push(what.into());
    }
    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
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
        if text == "trip" {
            if let Some(p) = self.policy.get() {
                p.trip_kill_switch("sess", "test: engaged mid-batch");
            }
        }
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
        self.note("clipboard_write");
        Ok(())
    }
    fn platform(&self) -> &'static str {
        "recorder"
    }
}

fn element(role: &str, name: &str, node: u64) -> ElementInfo {
    ElementInfo {
        role: role.into(),
        name: Some(name.into()),
        value_preview: None,
        secure: false,
        bounds: None,
        node_id: Some(node),
        state: Default::default(),
        semantic_intent: None,
        bound_state: None,
    }
}

struct Rig {
    /// Holds the kill switch's file: tripping it writes there, and a switch
    /// shared with the other tests of this process would stop all of them.
    _dir: tempfile::TempDir,
    client: InProcClient,
    policy: Arc<Policy>,
    rec: Arc<Recorder>,
}

impl Rig {
    fn new(mut cfg: PolicyConfig) -> Rig {
        let dir = tempfile::tempdir().expect("temp dir");
        cfg.kill_switch_file = dir.path().join("STOP");
        let rec = Arc::new(Recorder::default());
        let foreground = cfg.foreground;
        let backend: Arc<dyn InputBackend> = match foreground {
            Foreground::Never => Arc::new(ForegroundInput::new(rec.clone())),
            _ => rec.clone(),
        };
        let mut arena = SnapshotArena::new();
        arena.install(Snapshot {
            id: "s1".into(),
            app: Some("TestApp".into()),
            window: None,
            elements: [
                ("@e1".to_string(), element("text_field", "Username", 101)),
                ("@e4".to_string(), element("button", "Save", 104)),
            ]
            .into_iter()
            .collect(),
            skeleton: false,
        });
        let relay = StepRelay::new();
        let module: Arc<InputModule> = Arc::new(
            InputModule::new(
                backend,
                Arc::new(Mutex::new(arena)),
                InputPolicy {
                    autonomous: true,
                    foreground,
                    ..InputPolicy::default()
                },
            )
            .with_steps(relay.clone()),
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
        }
    }

    async fn call(&self, tool: &str, args: Value) -> Envelope {
        self.client.call(tool, args).await
    }

    /// `(tool, phase, decision)` of every audit record, in order.
    fn audit(&self) -> Vec<(String, String, Option<String>)> {
        self.policy
            .audit_sink()
            .memory_records()
            .iter()
            .map(|r| {
                (
                    r["tool"].as_str().unwrap_or_default().to_string(),
                    r["phase"].as_str().unwrap_or_default().to_string(),
                    r["decision"].as_str().map(str::to_string),
                )
            })
            .collect()
    }
}

fn config() -> PolicyConfig {
    test_policy(&[Category::Input])
}

fn with_role(mut cfg: PolicyConfig, role: RoleProfile) -> PolicyConfig {
    cfg.active_role = Some(role.name.clone());
    cfg.roles.insert(role.name.clone(), role);
    cfg
}

fn denying(tool: &str) -> RoleProfile {
    let mut r = RoleProfile::new("no-typing");
    r.denied_tools = vec![tool.into()];
    r
}

fn batch(steps: Value) -> Value {
    json!({ "steps": steps })
}

fn set(text: &str) -> Value {
    json!({"tool": "set_value", "args": {"ref": "@e1", "text": text}})
}

fn typed(text: &str) -> Value {
    json!({"tool": "keyboard_type", "args": {"text": text}})
}

fn code(env: &Envelope) -> ErrorCode {
    env.error.as_ref().expect("an error").code
}

#[tokio::test]
async fn every_step_goes_through_the_gate_and_is_audited_as_its_own_call() {
    let rig = Rig::new(config());
    let env = rig
        .call(
            "input_steps",
            batch(json!([
                set("alice"),
                json!({"tool": "keyboard_shortcut", "args": {"combo": "cmd+s"}})
            ])),
        )
        .await;
    assert!(env.ok, "{env:?}");
    assert_eq!(rig.rec.log(), ["set_value 101 alice", "key cmd+s"]);
    let tools: Vec<_> = rig
        .audit()
        .into_iter()
        .map(|(t, p, _)| format!("{t}:{p}"))
        .collect();
    assert_eq!(
        tools,
        [
            "input_steps:pre",
            "set_value:pre",
            "set_value:post",
            "keyboard_shortcut:pre",
            "keyboard_shortcut:post",
            "input_steps:post"
        ],
        "the batch is one entry around one entry per step"
    );
}

#[tokio::test]
async fn a_tool_the_role_denies_is_refused_inside_a_batch_as_it_is_alone() {
    let rig = Rig::new(with_role(config(), denying("keyboard_type")));
    let alone = rig.call("keyboard_type", json!({"text": "x"})).await;
    assert_eq!(code(&alone), ErrorCode::PolicyDenied);
    assert!(rig.rec.log().is_empty());

    let env = rig
        .call(
            "input_steps",
            batch(json!([set("a"), typed("x"), set("b")])),
        )
        .await;
    assert_eq!(code(&env), ErrorCode::PolicyDenied);
    let data = env.data.unwrap();
    assert_eq!(data["failed_at"], 1);
    assert_eq!(data["ran"], 2);
    assert_eq!(
        rig.rec.log(),
        ["set_value 101 a"],
        "the denied step and the one after it reached nothing"
    );
    assert!(
        rig.audit()
            .iter()
            .any(|(t, p, d)| t == "keyboard_type" && p == "pre" && d.as_deref() == Some("deny")),
        "the refusal is in the audit log under the tool's own name: {:?}",
        rig.audit()
    );
}

#[tokio::test]
async fn a_role_allow_list_must_name_every_tool_a_batch_uses() {
    let mut role = RoleProfile::new("narrow");
    role.allowed_tools = Some(vec!["input_steps".into(), "set_value".into()]);
    let rig = Rig::new(with_role(config(), role));
    let env = rig
        .call(
            "input_steps",
            batch(json!([set("a"), {"tool": "keyboard_shortcut", "args": {"combo": "cmd+s"}}])),
        )
        .await;
    assert_eq!(code(&env), ErrorCode::PolicyDenied);
    assert_eq!(rig.rec.log(), ["set_value 101 a"]);
}

#[tokio::test]
async fn a_tool_that_needs_consent_asks_for_it_in_a_batch_too() {
    let mut role = RoleProfile::new("careful");
    role.require_consent_for_tools = vec!["keyboard_shortcut".into()];
    let rig = Rig::new(with_role(config(), role));
    let step = json!({"tool": "keyboard_shortcut", "args": {"combo": "cmd+s"}});
    let alone = rig
        .call("keyboard_shortcut", json!({"combo": "cmd+s"}))
        .await;
    let env = rig
        .call("input_steps", batch(json!([set("a"), step])))
        .await;
    // Autonomous mode has no one to ask, so consent becomes a refusal, in the
    // batch exactly as alone.
    assert_eq!(code(&alone), ErrorCode::ConsentRequired);
    assert_eq!(code(&env), ErrorCode::ConsentRequired);
    assert_eq!(rig.rec.log(), ["set_value 101 a"]);
}

#[tokio::test]
async fn a_category_that_is_not_enabled_refuses_the_batch_itself() {
    let rig = Rig::new(test_policy(&[Category::Vision]));
    let env = rig.call("input_steps", batch(json!([set("a")]))).await;
    assert_eq!(code(&env), ErrorCode::PolicyDenied);
    assert!(rig.rec.log().is_empty());
}

#[tokio::test]
async fn a_dry_run_batch_runs_no_step() {
    let mut cfg = config();
    cfg.mode = Mode::DryRun;
    let rig = Rig::new(cfg);
    let env = rig.call("input_steps", batch(json!([set("a")]))).await;
    assert_eq!(env.data.unwrap()["dry_run"], true);
    assert!(rig.rec.log().is_empty());
}

/// Nothing outside the input module can be named in a batch, so the policy
/// tier of the other categories is never in question: the step is refused
/// before anything runs, and the other tool is never even audited as a call.
#[tokio::test]
async fn a_dangerous_or_foreign_tool_in_a_batch_is_refused_before_any_step_runs() {
    let rig = Rig::new(config());
    for tool in [
        "exec",
        "browser_eval",
        "write_file",
        "input_steps",
        "clipboard_read",
    ] {
        let env = rig
            .call(
                "input_steps",
                batch(json!([set("a"), {"tool": tool, "args": {}}])),
            )
            .await;
        assert_eq!(code(&env), ErrorCode::InvalidArgs, "{tool}");
        assert!(env.error.unwrap().message.contains("step 1"), "{tool}");
    }
    assert!(
        rig.rec.log().is_empty(),
        "step 0 ran although step 1 was bad"
    );
    assert!(
        !rig.audit()
            .iter()
            .any(|(t, _, _)| t == "exec" || t == "write_file"),
        "{:?}",
        rig.audit()
    );
}

#[tokio::test]
async fn foreground_never_refuses_the_whole_batch_and_nothing_reaches_the_backend() {
    let mut cfg = config();
    cfg.foreground = Foreground::Never;
    let rig = Rig::new(cfg);
    // Alone, the keystroke is refused; the set_value in front of it is not.
    let alone = rig.call("keyboard_type", json!({"text": "y"})).await;
    assert_eq!(code(&alone), ErrorCode::PolicyDenied);
    let env = rig
        .call("input_steps", batch(json!([set("x"), typed("y")])))
        .await;
    assert_eq!(code(&env), ErrorCode::PolicyDenied);
    let msg = env.error.unwrap().message;
    assert!(
        msg.contains("foreground = \"never\"") && msg.contains("step 1"),
        "{msg}"
    );
    assert!(rig.rec.log().is_empty(), "{:?}", rig.rec.log());

    // A batch of steps that are each background-safe alone is allowed.
    let env = rig
        .call(
            "input_steps",
            batch(
                json!([set("x"), {"tool": "ui_action", "args": {"ref": "@e4", "action": "click"}}]),
            ),
        )
        .await;
    assert!(env.ok, "{env:?}");
    assert_eq!(rig.rec.log(), ["set_value 101 x", "perform 104 Click"]);
}

#[tokio::test]
async fn the_kill_switch_engaging_mid_batch_stops_the_steps_after_it() {
    let rig = Rig::new(config());
    let env = rig
        .call(
            "input_steps",
            batch(json!([typed("trip"), set("late"), set("later")])),
        )
        .await;
    assert!(!env.ok);
    let data = env.data.unwrap();
    assert_eq!(data["ran"], 2, "step 0 ran, step 1 was turned away");
    assert_eq!(data["failed_at"], 1);
    assert_eq!(rig.rec.log(), ["type trip"]);
    assert!(
        rig.audit()
            .iter()
            .any(|(t, _, d)| t == "set_value" && d.as_deref() == Some("kill_switch")),
        "{:?}",
        rig.audit()
    );
}

#[tokio::test]
async fn secret_text_in_a_step_stays_out_of_every_audit_record() {
    let rig = Rig::new(config());
    let env = rig
        .call(
            "input_steps",
            batch(
                json!([{"tool": "keyboard_type", "args": {"text": "hunter2-pw", "secret": true}}]),
            ),
        )
        .await;
    assert!(env.ok, "{env:?}");
    assert_eq!(rig.rec.log(), ["type hunter2-pw"], "the field did get it");
    let all = serde_json::to_string(&rig.policy.audit_sink().memory_records()).unwrap();
    assert!(
        !all.contains("hunter2-pw"),
        "the secret reached the audit log: {all}"
    );
    assert!(all.contains("redacted"), "{all}");
}

/// A denial inside a batch is a denial: it counts against the budget and is
/// audited, the same as the single call.
#[tokio::test]
async fn a_denied_step_counts_against_the_denial_budget() {
    let rig = Rig::new(with_role(config(), denying("keyboard_type")));
    let before = rig.policy.denial_count();
    rig.call("keyboard_type", json!({"text": "x"})).await;
    let alone = rig.policy.denial_count() - before;
    let before = rig.policy.denial_count();
    rig.call("input_steps", batch(json!([typed("x")]))).await;
    assert_eq!(rig.policy.denial_count() - before, alone);
    assert!(alone >= 1);
}

#[tokio::test]
async fn a_protected_path_in_a_step_is_caught_by_the_invariants_as_it_is_alone() {
    let rig = Rig::new(config());
    let alone = rig
        .call("keyboard_type", json!({"text": "cat /etc/shadow"}))
        .await;
    let env = rig
        .call(
            "input_steps",
            batch(json!([set("a"), typed("cat /etc/shadow")])),
        )
        .await;
    assert_eq!(alone.ok, env.ok);
    assert_eq!(code(&alone), ErrorCode::PolicyDenied, "{alone:?}");
    assert_eq!(code(&env), ErrorCode::PolicyDenied, "{env:?}");
    assert!(rig.rec.log().is_empty(), "nothing in the batch ran");
}

#[tokio::test]
async fn after_shutdown_a_batch_has_no_way_back_and_runs_nothing() {
    let rig = Rig::new(config());
    rig.client.server().shutdown();
    let env = rig.call("input_steps", batch(json!([set("a")]))).await;
    assert!(!env.ok);
    assert!(env.error.unwrap().message.contains("not connected"));
    assert!(rig.rec.log().is_empty());
}

/// A synthetic token stands for personal data and is resolved only where the
/// tool is a listed sink. `input_steps` is not one, so a token inside a batch
/// is refused rather than resolved: the safe direction, since the plaintext
/// then never travels through a path that was not designed for it.
#[tokio::test]
async fn a_personal_data_token_is_resolved_alone_and_refused_in_a_batch() {
    let rig = Rig::new(config());
    rig.policy.set_anonymize_enabled(true);
    rig.policy
        .register_entity("Alice Example", mcp_policy::EntityType::PersonName);
    let mut probe = json!({"text": "Alice Example"});
    rig.policy.anonymize(&mut probe);
    let token = probe["text"].as_str().unwrap().to_string();
    assert_ne!(token, "Alice Example");

    let alone = rig.call("keyboard_type", json!({"text": token})).await;
    assert!(alone.ok, "{alone:?}");
    assert_eq!(rig.rec.log(), ["type Alice Example"]);

    let env = rig.call("input_steps", batch(json!([typed(&token)]))).await;
    assert_eq!(code(&env), ErrorCode::PolicyDenied);
    assert_eq!(rig.rec.log().len(), 1, "the batch typed nothing");
}
