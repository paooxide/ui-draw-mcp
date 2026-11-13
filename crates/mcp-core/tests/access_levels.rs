//! The three levels through the real server: what `tools/list` advertises,
//! what a call can reach, and that a dry run performs nothing, for every level.
//! Configs come from the real loader, so this is also a test that what the
//! operator writes is what the server enforces.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use mcp_core::{Registry, Server};
use mcp_policy::{Access, AuditSink, Policy, PolicyConfig, Redactor};
use mcp_types::{CallCtx, Category, Envelope, ErrorCode, Tier, ToolDescriptor, ToolModule};
use serde_json::{json, Value};

struct Mock {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl ToolModule for Mock {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        let schema = json!({ "type": "object", "properties": {}, "required": [] });
        vec![
            ToolDescriptor::new(
                "read_probe",
                Category::Vision,
                Tier::Read,
                "read",
                schema.clone(),
            ),
            ToolDescriptor::new(
                "vision_change",
                Category::Vision,
                Tier::Standard,
                "standard",
                schema.clone(),
            ),
            ToolDescriptor::new(
                "vision_danger",
                Category::Vision,
                Tier::Dangerous,
                "dangerous",
                schema.clone(),
            ),
            ToolDescriptor::new(
                "term_run",
                Category::Terminal,
                Tier::Standard,
                "terminal",
                schema,
            ),
        ]
    }

    async fn call(&self, name: &str, _args: Value, _ctx: &CallCtx) -> Envelope {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Envelope::ok(name, json!({ "ran": true }))
    }
}

fn server(config: PolicyConfig) -> (Server, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let registry = Registry::build(vec![Arc::new(Mock {
        calls: calls.clone(),
    })])
    .unwrap();
    let policy = Arc::new(Policy::new(config, AuditSink::memory(), Redactor::empty()));
    (Server::new(registry, policy, "levels"), calls)
}

fn load(text: &str) -> PolicyConfig {
    PolicyConfig::from_toml_str(text).unwrap_or_else(|e| panic!("{e}\n{text}"))
}

async fn advertised(server: &Server) -> Vec<String> {
    let out = server
        .handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

/// A category the file does not list is neither advertised nor callable, at
/// every level, and the engine behind it is never reached.
#[tokio::test]
async fn an_unlisted_category_is_neither_advertised_nor_callable_at_any_level() {
    for level in ["ask", "auto", "bypass"] {
        let (server, calls) = server(load(&format!(
            "[policy]\naccess = \"{level}\"\ncategories = [\"vision\"]\n"
        )));
        let names = advertised(&server).await;
        assert!(
            names.contains(&"read_probe".to_string()),
            "{level}: {names:?}"
        );
        assert!(
            names.contains(&"vision_danger".to_string()),
            "{level}: {names:?}"
        );
        assert!(
            !names.contains(&"term_run".to_string()),
            "{level}: {names:?}"
        );

        let env = server.dispatch_call("term_run", json!({})).await;
        assert!(!env.ok, "{level}");
        assert_eq!(env.error.unwrap().code, ErrorCode::PolicyDenied, "{level}");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "{level}: the engine ran");

        // A listed category still works.
        let env = server.dispatch_call("read_probe", json!({})).await;
        assert!(env.ok, "{level}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}

/// With no list, the level turns every category on, as before.
#[tokio::test]
async fn without_a_category_list_every_category_is_advertised_at_every_level() {
    for level in ["ask", "auto", "bypass"] {
        let (server, _) = server(load(&format!("[policy]\naccess = \"{level}\"\n")));
        let names = advertised(&server).await;
        assert_eq!(names.len(), 4, "{level}: {names:?}");
    }
}

/// Dry run under each level: a read runs, nothing else does. Whichever way it
/// is asked for (`dry_run`, the older `mode`), and including the dangerous
/// tool, which a rehearsal under `ask` refuses rather than prompting for.
#[tokio::test]
async fn dry_run_performs_nothing_at_any_level() {
    for level in ["ask", "auto", "bypass"] {
        for spelling in ["dry_run = true", "mode = \"dry_run\""] {
            let (server, calls) = server(load(&format!(
                "[policy]\naccess = \"{level}\"\ncategories = [\"vision\", \"terminal\"]\n{spelling}\n"
            )));
            let label = format!("{level} {spelling}");

            let env = server.dispatch_call("read_probe", json!({})).await;
            assert!(env.ok, "{label}");
            assert_eq!(calls.load(Ordering::SeqCst), 1, "{label}: the read runs");

            for name in ["vision_change", "term_run"] {
                let env = server.dispatch_call(name, json!({})).await;
                assert!(env.ok, "{label} {name}: a rehearsal reports");
                let d = env.data.unwrap();
                assert_eq!(d["dry_run"], json!(true), "{label} {name}");
                assert_eq!(d["would_execute"]["tool"], json!(name));
            }
            let env = server.dispatch_call("vision_danger", json!({})).await;
            match level {
                // Asks a human, and a rehearsal never prompts: refused.
                "ask" => {
                    assert!(!env.ok, "{label}");
                    assert_eq!(env.error.unwrap().code, ErrorCode::ConsentRequired);
                }
                _ => assert_eq!(env.data.unwrap()["dry_run"], json!(true), "{label}"),
            }
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "{label}: nothing but the read reached the engine"
            );
        }
    }
}

/// The flag spelling, through the loader's flag entry point.
#[tokio::test]
async fn the_dry_run_flag_works_over_a_config_with_no_dry_run_in_it() {
    let dir = std::env::temp_dir().join(format!("agentctl-levels-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    std::fs::write(&path, "[policy]\naccess = \"auto\"\n").unwrap();
    let cfg = PolicyConfig::load_from_with(&path, None, true).unwrap();
    assert_eq!(cfg.access, Some(Access::Auto));
    let (server, calls) = server(cfg);
    let env = server.dispatch_call("term_run", json!({})).await;
    assert_eq!(env.data.unwrap()["dry_run"], json!(true));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    std::fs::remove_dir_all(&dir).ok();
}
