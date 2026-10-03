//! `notifications/cancelled` over the HTTP transport.
//!
//! One POST carries one request, so a cancel can only reach a running call if
//! the server is serving a second connection while the first is still open.
//! The module here polls its cancel token as the glide loop does; what is under
//! test is the transport and server plumbing, not an OS engine.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use mcp_core::{generate_token, HttpConfig, HttpTransport, Registry, Server};
use mcp_policy::{AuditSink, Mode, Policy, PolicyConfig, Redactor};
use mcp_types::{CallCtx, Category, Envelope, Tier, ToolDescriptor, ToolModule};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

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

struct Fixture {
    addr: std::net::SocketAddr,
    token: String,
    server: Arc<Server>,
    _dir: tempfile::TempDir,
}

async fn start(max_connections: usize) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let cfg = PolicyConfig {
        categories: vec![Category::Vision],
        mode: Mode::Autonomous,
        kill_switch_file: dir.path().join("STOP"),
        ..PolicyConfig::default()
    };
    let registry = Registry::build(vec![Arc::new(Spinner)]).unwrap();
    let policy = Arc::new(Policy::new(cfg, AuditSink::memory(), Redactor::empty()));
    let server = Arc::new(Server::new(registry, policy, "http-cancel"));
    let token = generate_token().unwrap();
    let transport = HttpTransport::bind(HttpConfig {
        bind: ([127, 0, 0, 1], 0).into(),
        token: token.clone(),
        max_connections,
        ..HttpConfig::default()
    })
    .await
    .unwrap();
    let addr = transport.local_addr().unwrap();
    let s = server.clone();
    tokio::spawn(async move { transport.serve(s).await });
    Fixture {
        addr,
        token,
        server,
        _dir: dir,
    }
}

async fn post(addr: std::net::SocketAddr, token: &str, body: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n\
         Authorization: Bearer {token}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut out = String::new();
    stream.read_to_string(&mut out).await.unwrap();
    out
}

fn status(response: &str) -> u16 {
    response
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0)
}

fn body_json(response: &str) -> Value {
    let at = response.find("\r\n\r\n").unwrap();
    serde_json::from_str(&response[at + 4..]).unwrap()
}

const CALL: &str = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"spin_wait","arguments":{}}}"#;
const CANCEL: &str =
    r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":7}}"#;

async fn wait_inflight(server: &Server, n: usize) {
    let until = Instant::now() + Duration::from_secs(3);
    while server.inflight_count() != n {
        assert!(Instant::now() < until, "inflight never reached {n}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn post_records(server: &Server) -> usize {
    server
        .policy()
        .audit_sink()
        .memory_records()
        .iter()
        .filter(|r| r["phase"] == "post" && r["tool"] == "spin_wait")
        .count()
}

#[tokio::test]
async fn a_cancel_post_reaches_a_call_running_in_another_post() {
    let f = start(8).await;
    let (addr, token) = (f.addr, f.token.clone());
    let started = Instant::now();
    let call = tokio::spawn(async move { post(addr, &token, CALL).await });
    wait_inflight(&f.server, 1).await;

    let ack = post(f.addr, &f.token, CANCEL).await;
    assert_eq!(status(&ack), 202);

    let first = call.await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the call was not cancelled promptly"
    );
    // The client withdrew the request, so it is not answered with a result.
    assert_eq!(status(&first), 202, "{first}");
    assert!(!first.contains("\"result\""), "{first}");
    assert_eq!(f.server.inflight_count(), 0);
    // ...but the audit log still shows the call ending.
    assert_eq!(post_records(&f.server), 1);
}

#[tokio::test]
async fn a_cancel_post_without_the_token_does_not_reach_the_call() {
    let f = start(8).await;
    let (addr, token) = (f.addr, f.token.clone());
    let call = tokio::spawn(async move { post(addr, &token, CALL).await });
    wait_inflight(&f.server, 1).await;

    let refused = post(f.addr, "wrong-token-wrong-token", CANCEL).await;
    assert_eq!(status(&refused), 401);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        f.server.inflight_count(),
        1,
        "an unauthenticated cancel got through"
    );

    // Clean up with a real cancel.
    post(f.addr, &f.token, CANCEL).await;
    call.await.unwrap();
}

#[tokio::test]
async fn a_kill_switch_stop_is_still_answered_over_http() {
    let f = start(8).await;
    let (addr, token) = (f.addr, f.token.clone());
    let call = tokio::spawn(async move { post(addr, &token, CALL).await });
    wait_inflight(&f.server, 1).await;
    f.server
        .policy()
        .trip_kill_switch("http-cancel", "human took over");
    let first = call.await.unwrap();
    assert_eq!(status(&first), 200, "{first}");
    let v = body_json(&first);
    let text = v["result"]["content"][0]["text"].as_str().unwrap();
    let env: Value = serde_json::from_str(text).unwrap();
    assert_eq!(env["data"]["cancelled"], true);
}

#[tokio::test]
async fn connections_past_the_bound_are_refused_not_queued() {
    let f = start(1).await;
    // Occupy the only slot with a connection that never finishes its request.
    let mut hog = TcpStream::connect(f.addr).await.unwrap();
    hog.write_all(b"POST / HTTP/1.1\r\n").await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let refused = post(
        f.addr,
        &f.token,
        r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
    )
    .await;
    assert_eq!(status(&refused), 503, "{refused}");
    drop(hog);
}
