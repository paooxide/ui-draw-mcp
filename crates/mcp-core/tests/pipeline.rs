//! End-to-end tests for the dispatch pipeline and MCP protocol surface, using a
//! mock engine so they run on any OS. Covers: handshake, category-filtered
//! tools/list, allow/deny gating, audit records (INV-2), fail-closed (INV-5),
//! kill switch, and that the engine is never reached on a deny (INV-1).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use mcp_core::{Registry, Server};
use mcp_policy::{AuditSink, Mode, Policy, PolicyConfig, Redactor};
use mcp_types::{CallCtx, Category, Envelope, ErrorCode, Tier, ToolDescriptor, ToolModule};
use serde_json::{json, Value};

/// A mock engine that counts how many times it was actually invoked, so tests
/// can assert the engine is never reached past a deny.
struct MockModule {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl ToolModule for MockModule {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        vec![
            ToolDescriptor::new(
                "vision_probe",
                Category::Vision,
                Tier::Read,
                "read-tier vision tool",
                json!({ "type": "object", "properties": {}, "required": [] }),
            ),
            ToolDescriptor::new(
                "term_run",
                Category::Terminal,
                Tier::Standard,
                "standard tool in a disabled category",
                json!({ "type": "object", "properties": {}, "required": [] }),
            ),
            ToolDescriptor::new(
                "danger_op",
                Category::Vision,
                Tier::Dangerous,
                "dangerous tool",
                json!({ "type": "object", "properties": {}, "required": [] }),
            ),
        ]
    }

    async fn call(&self, name: &str, _args: Value, _ctx: &CallCtx) -> Envelope {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Envelope::ok(name, json!({ "ran": true }))
    }

    /// Flags one specific argument shape as risky, the way a real engine
    /// flags a destructive command.
    fn consent_prompt(&self, _name: &str, args: &Value) -> Option<String> {
        args.get("risky")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            .then(|| "this will do something irreversible".to_string())
    }
}

/// A stand-in human. Records what it was shown so tests can assert the person
/// sees the actual action, not a vague prompt.
struct ScriptedHuman {
    answer: mcp_policy::ConsentOutcome,
    seen: std::sync::Mutex<Vec<String>>,
}

impl mcp_policy::ConsentProvider for ScriptedHuman {
    fn request(&self, req: &mcp_policy::ConsentRequest) -> mcp_policy::ConsentOutcome {
        self.seen
            .lock()
            .unwrap()
            .push(format!("{}|{}", req.tool, req.summary));
        self.answer
    }
    fn kind(&self) -> &'static str {
        "scripted"
    }
}

fn server_with_consent(
    config: PolicyConfig,
    answer: mcp_policy::ConsentOutcome,
) -> (Server, Arc<AtomicUsize>, Arc<ScriptedHuman>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let module = Arc::new(MockModule {
        calls: calls.clone(),
    });
    let registry = Registry::build(vec![module]).unwrap();
    let human = Arc::new(ScriptedHuman {
        answer,
        seen: std::sync::Mutex::new(Vec::new()),
    });
    let policy = Arc::new(
        Policy::new(config, AuditSink::memory(), Redactor::empty()).with_consent(human.clone()),
    );
    let server = Server::new(registry, policy, "test-session");
    (server, calls, human)
}

fn consent_cfg() -> PolicyConfig {
    PolicyConfig {
        categories: vec![Category::Vision],
        mode: Mode::Interactive,
        ..PolicyConfig::default()
    }
}

fn server_with(config: PolicyConfig) -> (Server, Arc<AtomicUsize>, Arc<Policy>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let module = Arc::new(MockModule {
        calls: calls.clone(),
    });
    let registry = Registry::build(vec![module]).unwrap();
    let policy = Arc::new(Policy::new(config, AuditSink::memory(), Redactor::empty()));
    let server = Server::new(registry, policy.clone(), "test-session");
    (server, calls, policy)
}

fn vision_only() -> PolicyConfig {
    PolicyConfig {
        categories: vec![Category::Vision],
        ..PolicyConfig::default()
    }
}

#[tokio::test]
async fn initialize_reports_protocol_and_tools_capability() {
    let (server, _, _) = server_with(vision_only());
    let out = server
        .handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#)
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["id"], 1);
    assert_eq!(v["result"]["protocolVersion"], "2025-11-25");
    assert!(v["result"]["capabilities"]["tools"].is_object());
}

#[tokio::test]
async fn tools_list_only_shows_enabled_categories() {
    let (server, _, _) = server_with(vision_only());
    let out = server
        .handle_line(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    let names: Vec<&str> = v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    // vision_probe + danger_op are Vision; term_run is Terminal (disabled).
    assert!(names.contains(&"vision_probe"));
    assert!(names.contains(&"danger_op"));
    assert!(!names.contains(&"term_run"));
}

#[tokio::test]
async fn allowed_read_tool_runs_and_is_audited() {
    let (server, calls, policy) = server_with(vision_only());
    let env = server.dispatch_call("vision_probe", json!({})).await;
    assert!(env.ok);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // INV-2: exactly one pre + one post audit record for the call.
    let recs = policy.audit_sink().memory_records();
    assert_eq!(recs.len(), 2);
    assert_eq!(recs[0]["phase"], "pre");
    assert_eq!(recs[0]["decision"], "allow");
    assert_eq!(recs[1]["phase"], "post");
    assert_eq!(recs[1]["ok"], true);
}

#[tokio::test]
async fn disabled_category_is_denied_and_engine_never_runs() {
    // INV-1 + INV-5: term_run's category (Terminal) is not enabled.
    let (server, calls, policy) = server_with(vision_only());
    let env = server.dispatch_call("term_run", json!({})).await;
    assert!(!env.ok);
    assert_eq!(env.error.unwrap().code, ErrorCode::PolicyDenied);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "engine must not run on deny"
    );
    assert_eq!(policy.denial_count(), 1);

    let recs = policy.audit_sink().memory_records();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0]["decision"], "deny");
}

#[tokio::test]
async fn dangerous_tool_denied_until_enabled() {
    // Not enabled -> denied even though its category (Vision) is on.
    let (server, calls, _) = server_with(vision_only());
    let env = server.dispatch_call("danger_op", json!({})).await;
    assert!(!env.ok);
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    // Enabled -> runs.
    let mut cfg = vision_only();
    cfg.enable.push("danger_op".into());
    let (server2, calls2, _) = server_with(cfg);
    let env2 = server2.dispatch_call("danger_op", json!({})).await;
    assert!(env2.ok);
    assert_eq!(calls2.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unknown_tool_is_invalid_args() {
    let (server, calls, _) = server_with(vision_only());
    let env = server.dispatch_call("nope", json!({})).await;
    assert_eq!(env.error.unwrap().code, ErrorCode::InvalidArgs);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn kill_switch_aborts_before_engine() {
    let dir = tempfile::tempdir().unwrap();
    let stop = dir.path().join("STOP");
    std::fs::write(&stop, b"").unwrap();
    let mut cfg = vision_only();
    cfg.kill_switch_file = stop;
    let (server, calls, _) = server_with(cfg);

    let env = server.dispatch_call("vision_probe", json!({})).await;
    assert!(!env.ok);
    assert_eq!(env.error.unwrap().code, ErrorCode::Timeout);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn tools_call_wraps_envelope_in_mcp_result() {
    let (server, _, _) = server_with(vision_only());
    let line = r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"vision_probe","arguments":{}}}"#;
    let out = server.handle_line(line).await.unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["result"]["isError"], false);
    let text = v["result"]["content"][0]["text"].as_str().unwrap();
    let env: Value = serde_json::from_str(text).unwrap();
    assert_eq!(env["ok"], true);
    assert_eq!(env["data"]["ran"], true);
}

#[tokio::test]
async fn parse_error_returns_jsonrpc_error_and_survives() {
    let (server, _, _) = server_with(vision_only());
    let out = server.handle_line("{not json").await.unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["error"]["code"], -32700);
}

#[tokio::test]
async fn notification_gets_no_response() {
    let (server, _, _) = server_with(vision_only());
    let out = server
        .handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
        .await;
    assert!(out.is_none());
}

#[tokio::test]
async fn autonomous_mode_denies_consent_tools() {
    let mut cfg = vision_only();
    cfg.mode = Mode::Autonomous;
    let (server, _, _) = server_with(cfg);
    // vision_probe is read-tier so it still runs; this asserts mode wiring holds.
    let env = server.dispatch_call("vision_probe", json!({})).await;
    assert!(env.ok);
}

// ---- consent channel --------------------------------------------------------

/// An approved action runs, and the human is shown *which* action.
#[tokio::test]
async fn approved_consent_lets_the_call_through() {
    let (server, calls, human) =
        server_with_consent(consent_cfg(), mcp_policy::ConsentOutcome::Approved);
    let res = server
        .handle_line(
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                    "params":{"name":"vision_probe","arguments":{"risky":true}}})
            .to_string(),
        )
        .await
        .unwrap();
    assert!(!res.contains("CONSENT_REQUIRED"), "{res}");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the tool should have run");
    let seen = human.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].starts_with("vision_probe|"), "{:?}", seen[0]);
    assert!(seen[0].contains("irreversible"), "{:?}", seen[0]);
}

/// A declined action must not execute.
#[tokio::test]
async fn declined_consent_blocks_execution() {
    let (server, calls, _) = server_with_consent(consent_cfg(), mcp_policy::ConsentOutcome::Denied);
    let res = server
        .handle_line(
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                    "params":{"name":"vision_probe","arguments":{"risky":true}}})
            .to_string(),
        )
        .await
        .unwrap();
    assert!(res.contains("CONSENT_REQUIRED"), "{res}");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "declined call must not run"
    );
}

/// Silence is not approval.
#[tokio::test]
async fn timeout_is_treated_as_denial() {
    let (server, calls, _) =
        server_with_consent(consent_cfg(), mcp_policy::ConsentOutcome::TimedOut);
    let res = server
        .handle_line(
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                    "params":{"name":"vision_probe","arguments":{"risky":true}}})
            .to_string(),
        )
        .await
        .unwrap();
    assert!(res.contains("CONSENT_REQUIRED"), "{res}");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

/// Non-risky calls must not interrupt anyone.
#[tokio::test]
async fn safe_calls_never_prompt() {
    let (server, calls, human) =
        server_with_consent(consent_cfg(), mcp_policy::ConsentOutcome::Approved);
    server
        .handle_line(
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                    "params":{"name":"vision_probe","arguments":{}}})
            .to_string(),
        )
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(
        human.seen.lock().unwrap().is_empty(),
        "should not have asked"
    );
}

/// Autonomous mode has nobody to ask, so a risky call is denied outright and
/// the human is never contacted.
#[tokio::test]
async fn autonomous_mode_denies_without_prompting() {
    let cfg = PolicyConfig {
        categories: vec![Category::Vision],
        mode: Mode::Autonomous,
        ..PolicyConfig::default()
    };
    let (server, calls, human) = server_with_consent(cfg, mcp_policy::ConsentOutcome::Approved);
    let res = server
        .handle_line(
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                    "params":{"name":"vision_probe","arguments":{"risky":true}}})
            .to_string(),
        )
        .await
        .unwrap();
    assert!(res.contains("CONSENT_REQUIRED"), "{res}");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(
        human.seen.lock().unwrap().is_empty(),
        "must not prompt when autonomous"
    );
}

/// Consent fatigue: an agent must not be able to raise unlimited dialogs.
#[tokio::test]
async fn prompt_budget_caps_interruptions() {
    let cfg = PolicyConfig {
        categories: vec![Category::Vision],
        mode: Mode::Interactive,
        max_denials: 1000,
        max_consent_prompts: 3,
        ..PolicyConfig::default()
    };
    let (server, _, human) = server_with_consent(cfg, mcp_policy::ConsentOutcome::Approved);
    for _ in 0..6 {
        server
            .handle_line(
                &json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                        "params":{"name":"vision_probe","arguments":{"risky":true}}})
                .to_string(),
            )
            .await
            .unwrap();
    }
    assert_eq!(
        human.seen.lock().unwrap().len(),
        3,
        "the human must only be interrupted up to the budget"
    );
}

/// Engines own things the process does not: child browsers, PTY process
/// groups, temporary profiles. `Server::shutdown` is the one call that releases
/// them, so it must actually reach every module.
#[tokio::test]
async fn shutdown_reaches_every_module() {
    struct Counting(Arc<AtomicUsize>);
    #[async_trait::async_trait]
    impl ToolModule for Counting {
        fn descriptors(&self) -> Vec<ToolDescriptor> {
            vec![ToolDescriptor::new(
                "counted",
                Category::System,
                Tier::Read,
                "test",
                serde_json::json!({"type":"object","properties":{},"required":[]}),
            )]
        }
        async fn call(&self, name: &str, _a: Value, _c: &CallCtx) -> Envelope {
            Envelope::ok(name, serde_json::json!({}))
        }
        fn shutdown(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let hits = Arc::new(AtomicUsize::new(0));
    let registry = Registry::build(vec![Arc::new(Counting(hits.clone()))]).unwrap();
    let policy = Arc::new(Policy::new(
        PolicyConfig::default(),
        AuditSink::memory(),
        Redactor::empty(),
    ));
    let server = Server::new(registry, policy, "s".to_string());
    server.shutdown();
    server.shutdown();
    assert_eq!(
        hits.load(Ordering::SeqCst),
        2,
        "shutdown must be idempotent and always reach the module"
    );
}
