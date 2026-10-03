//! The per-call cancel token must actually be tripped: by the kill switch (the
//! STOP file or an in-process trip) and by MCP `notifications/cancelled`.
//!
//! The engine here polls `ctx.cancel` exactly as the glide loop does. It stands
//! in for the *engine*, not for the OS: what is under test is that the server
//! trips the token, which is pure protocol and policy logic.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use mcp_core::{Registry, Server};
use mcp_policy::{AuditSink, Mode, Policy, PolicyConfig, Redactor};
use mcp_types::{CallCtx, Category, Envelope, Tier, ToolDescriptor, ToolModule};
use serde_json::{json, Value};
use tokio::io::BufReader;

/// Runs until its token is cancelled, or ten seconds pass.
struct Spinner;

#[async_trait]
impl ToolModule for Spinner {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        vec![ToolDescriptor::new(
            "spin_wait",
            Category::Vision,
            Tier::Read,
            "polls its cancel token",
            json!({ "type": "object", "properties": {}, "required": [] }),
        )]
    }

    async fn call(&self, name: &str, _args: Value, ctx: &CallCtx) -> Envelope {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if ctx.cancel.is_cancelled() {
                return Envelope::ok(name, json!({ "cancelled": true }));
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Envelope::ok(name, json!({ "cancelled": false }))
    }
}

fn server_in(dir: &std::path::Path) -> (Arc<Server>, std::path::PathBuf) {
    let stop = dir.join("STOP");
    let cfg = PolicyConfig {
        categories: vec![Category::Vision],
        mode: Mode::Autonomous,
        kill_switch_file: stop.clone(),
        ..PolicyConfig::default()
    };
    let registry = Registry::build(vec![Arc::new(Spinner)]).unwrap();
    let policy = Arc::new(Policy::new(cfg, AuditSink::memory(), Redactor::empty()));
    (
        Arc::new(Server::new(registry, policy, "cancel-session")),
        stop,
    )
}

fn call_line(id: i64) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"spin_wait","arguments":{{}}}}}}"#
    )
}

fn cancel_line(id: Value) -> String {
    json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":id}})
        .to_string()
}

fn was_cancelled(response: &str) -> bool {
    let v: Value = serde_json::from_str(response).unwrap();
    let text = v["result"]["content"][0]["text"].as_str().unwrap();
    let env: Value = serde_json::from_str(text).unwrap();
    env["data"]["cancelled"] == true
}

fn data_cancelled(env: &Envelope) -> bool {
    env.data.as_ref().map(|d| d["cancelled"] == true) == Some(true)
}

#[tokio::test]
async fn a_stop_file_created_mid_call_cancels_the_running_call() {
    let dir = tempfile::tempdir().unwrap();
    let (server, stop) = server_in(dir.path());
    let started = Instant::now();
    let touch = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        std::fs::write(&stop, "stop\n").unwrap();
    };
    let (env, ()) = tokio::join!(server.dispatch_call("spin_wait", json!({})), touch);
    assert!(data_cancelled(&env), "{env:?}");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "did not stop promptly"
    );
    assert_eq!(server.inflight_count(), 0);
}

#[tokio::test]
async fn an_in_process_trip_cancels_every_running_call() {
    let dir = tempfile::tempdir().unwrap();
    let (server, _stop) = server_in(dir.path());
    let started = Instant::now();
    let trip = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        server
            .policy()
            .trip_kill_switch("cancel-session", "human took over");
    };
    let (a, b, ()) = tokio::join!(
        server.dispatch_call("spin_wait", json!({})),
        server.dispatch_call("spin_wait", json!({})),
        trip
    );
    assert!(data_cancelled(&a));
    assert!(data_cancelled(&b));
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn a_cancel_notification_stops_only_the_named_request() {
    let dir = tempfile::tempdir().unwrap();
    let (server, _stop) = server_in(dir.path());
    let started = Instant::now();
    let cancel = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        // A string "1" is not the integer 1, and an unknown id is ignored.
        assert!(server.handle_line(&cancel_line(json!("1"))).await.is_none());
        assert!(server.handle_line(&cancel_line(json!(99))).await.is_none());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(server.inflight_count(), 2);
        server.handle_line(&cancel_line(json!(1))).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        // The other call is still running; stop it so the test can finish.
        assert_eq!(server.inflight_count(), 1);
        server.handle_line(&cancel_line(json!(2))).await;
    };
    let (l1, l2) = (call_line(1), call_line(2));
    let (one, two, ()) = tokio::join!(server.handle_line(&l1), server.handle_line(&l2), cancel);
    assert!(was_cancelled(&one.unwrap()));
    assert!(was_cancelled(&two.unwrap()));
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(server.inflight_count(), 0);
}

#[tokio::test]
async fn a_cancel_frame_is_read_from_the_stream_while_a_call_is_running() {
    let dir = tempfile::tempdir().unwrap();
    let (server, _stop) = server_in(dir.path());
    // Both frames are already buffered: a loop that reads one frame, runs it
    // to completion, and only then reads the next would sit here for ten
    // seconds. A trailing request shows ordering survives.
    let input = format!(
        "{}\n{}\n{}\n",
        call_line(1),
        cancel_line(json!(1)),
        r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#
    );
    let mut out: Vec<u8> = Vec::new();
    let started = Instant::now();
    server
        .serve_stream(BufReader::new(input.as_bytes()), &mut out)
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "cancel was not read mid-call"
    );
    let lines: Vec<&str> = std::str::from_utf8(&out).unwrap().lines().collect();
    assert_eq!(
        lines.len(),
        2,
        "one reply per request, none for the notification"
    );
    assert!(was_cancelled(lines[0]));
    let ping: Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(ping["id"], 3);
}
