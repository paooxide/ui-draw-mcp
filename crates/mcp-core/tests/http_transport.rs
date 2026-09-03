//! End-to-end tests for the optional HTTP transport, over a real TCP socket
//! with hand-written HTTP on the client side.
//!
//! The point of a raw client is that it can send things a well-behaved library
//! would refuse to: a duplicated `Content-Length`, a browser `Origin`, a body
//! that lies about its length. Those are the requests worth testing.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mcp_core::{generate_token, HttpConfig, HttpTransport, Registry, Server};
use mcp_policy::{AuditSink, Mode, Policy, PolicyConfig, Redactor};
use mcp_types::{CallCtx, Category, Envelope, Tier, ToolDescriptor, ToolModule};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

struct DemoModule;

#[async_trait]
impl ToolModule for DemoModule {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        vec![
            ToolDescriptor::new(
                "vision_ok",
                Category::Vision,
                Tier::Read,
                "an enabled read-tier tool",
                json!({ "type": "object", "properties": {}, "required": [] }),
            ),
            ToolDescriptor::new(
                "terminal_blocked",
                Category::Terminal,
                Tier::Standard,
                "a tool in a category that is not enabled",
                json!({ "type": "object", "properties": {}, "required": [] }),
            ),
        ]
    }

    async fn call(&self, name: &str, _args: Value, _ctx: &CallCtx) -> Envelope {
        Envelope::ok(name, json!({ "reached_engine": true }))
    }
}

/// A bound transport plus the address to talk to it on.
struct Fixture {
    addr: std::net::SocketAddr,
    token: String,
}

async fn start(allowed_origins: Vec<String>, max_body_bytes: usize) -> Fixture {
    let registry = Registry::build(vec![Arc::new(DemoModule)]).unwrap();
    let cfg = PolicyConfig {
        categories: vec![Category::Vision],
        mode: Mode::Autonomous,
        max_denials: usize::MAX,
        ..PolicyConfig::default()
    };
    let policy = Arc::new(Policy::new(cfg, AuditSink::memory(), Redactor::empty()));
    let server = Arc::new(Server::new(registry, policy, "http-session"));

    let token = generate_token().unwrap();
    let transport = HttpTransport::bind(HttpConfig {
        bind: ([127, 0, 0, 1], 0).into(),
        token: token.clone(),
        allowed_origins,
        max_body_bytes,
        read_timeout: Duration::from_secs(5),
    })
    .await
    .expect("must bind on loopback");
    let addr = transport.local_addr().unwrap();
    tokio::spawn(async move { transport.serve(server).await });
    Fixture { addr, token }
}

/// Send raw bytes, read the whole response. The server closes the connection
/// after one request, so read-to-end terminates on its own.
async fn raw(addr: std::net::SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream.write_all(request.as_bytes()).await.expect("write");
    stream.flush().await.expect("flush");
    let mut out = String::new();
    stream.read_to_string(&mut out).await.expect("read");
    out
}

fn post(token: Option<&str>, body: &str, extra: &str) -> String {
    let auth = match token {
        Some(t) => format!("Authorization: Bearer {t}\r\n"),
        None => String::new(),
    };
    format!(
        "POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n{auth}{extra}\r\n{body}",
        body.len()
    )
}

fn status(response: &str) -> u16 {
    response
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0)
}

fn body(response: &str) -> Value {
    let at = response
        .find("\r\n\r\n")
        .expect("response must have a body");
    serde_json::from_str(&response[at + 4..]).expect("body must be JSON")
}

const PING: &str = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;

// ---- configuration refusals ------------------------------------------------

/// Binding off-loopback puts a plaintext bearer token on the wire. Refuse at
/// bind time: a warning that scrolls past is not a control.
#[tokio::test]
async fn binding_off_loopback_is_refused() {
    for ip in [[0, 0, 0, 0], [192, 168, 1, 10]] {
        let err = HttpTransport::bind(HttpConfig {
            bind: (ip, 0).into(),
            token: generate_token().unwrap(),
            ..HttpConfig::default()
        })
        .await
        .expect_err("must refuse to bind");
        assert!(err.to_string().contains("loopback-only"), "{err}");
    }
}

/// There is no anonymous mode, and no "short token for local testing" mode.
#[tokio::test]
async fn a_weak_or_absent_token_is_refused() {
    for token in ["", "x", "hunter2", "0123456789abcde"] {
        let err = HttpTransport::bind(HttpConfig {
            bind: ([127, 0, 0, 1], 0).into(),
            token: token.to_string(),
            ..HttpConfig::default()
        })
        .await
        .expect_err("must refuse a weak token");
        assert!(err.to_string().contains("bearer token"), "{err}");
    }
}

// ---- authentication --------------------------------------------------------

#[tokio::test]
async fn a_valid_token_gets_a_jsonrpc_response() {
    let f = start(vec![], 1_000_000).await;
    let response = raw(f.addr, &post(Some(&f.token), PING, "")).await;
    assert_eq!(status(&response), 200);
    let v = body(&response);
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], 1);
    assert!(v["result"].is_object());
}

#[tokio::test]
async fn a_missing_wrong_or_malformed_token_is_rejected() {
    let f = start(vec![], 1_000_000).await;
    let wrong = "f".repeat(64);
    let cases: Vec<String> = vec![
        post(None, PING, ""),
        post(Some(&wrong), PING, ""),
        // Right value, wrong scheme.
        format!(
            "POST / HTTP/1.1\r\nContent-Length: {}\r\nAuthorization: Basic {}\r\n\r\n{PING}",
            PING.len(),
            f.token
        ),
        // A prefix of the real token must not pass.
        post(Some(&f.token[..32]), PING, ""),
    ];
    for request in cases {
        let response = raw(f.addr, &request).await;
        assert_eq!(status(&response), 401, "should be unauthorized: {request}");
        assert!(
            response.contains("WWW-Authenticate: Bearer"),
            "401 must say how to authenticate"
        );
    }
}

// ---- browser containment ---------------------------------------------------

/// The DNS-rebinding case. A page on any site can make the browser POST to
/// `127.0.0.1`; being local is not the same as being private. A real MCP client
/// sends no `Origin`, so refusing every unlisted one costs nothing.
#[tokio::test]
async fn a_request_carrying_an_origin_is_refused_even_with_a_valid_token() {
    let f = start(vec![], 1_000_000).await;
    for origin in [
        "https://evil.example",
        "http://localhost:3000",
        "null",
        "http://127.0.0.1:1234",
    ] {
        let request = post(Some(&f.token), PING, &format!("Origin: {origin}\r\n"));
        let response = raw(f.addr, &request).await;
        assert_eq!(status(&response), 403, "origin must be refused: {origin}");
    }
}

/// The escape hatch exists for a deployment that genuinely fronts this with a
/// local web UI, and it is exact-match only.
#[tokio::test]
async fn an_explicitly_allowed_origin_is_accepted() {
    let f = start(vec!["http://localhost:3000".into()], 1_000_000).await;
    let ok = post(Some(&f.token), PING, "Origin: http://localhost:3000\r\n");
    assert_eq!(status(&raw(f.addr, &ok).await), 200);

    // A near-miss is still a different origin.
    for near in [
        "http://localhost:3001",
        "https://localhost:3000",
        "http://localhost:3000.evil.example",
    ] {
        let request = post(Some(&f.token), PING, &format!("Origin: {near}\r\n"));
        assert_eq!(
            status(&raw(f.addr, &request).await),
            403,
            "near-miss origin must be refused: {near}"
        );
    }
}

// ---- protocol surface ------------------------------------------------------

#[tokio::test]
async fn only_post_to_the_endpoint_is_served() {
    let f = start(vec![], 1_000_000).await;
    let get = format!(
        "GET / HTTP/1.1\r\nAuthorization: Bearer {}\r\n\r\n",
        f.token
    );
    // GET is the SSE stream in the full spec; we do not implement it, and
    // saying so beats leaving the client waiting on a stream that never opens.
    assert_eq!(status(&raw(f.addr, &get).await), 405);

    let wrong_path = format!(
        "POST /admin HTTP/1.1\r\nContent-Length: {}\r\nAuthorization: Bearer {}\r\n\r\n{PING}",
        PING.len(),
        f.token
    );
    assert_eq!(status(&raw(f.addr, &wrong_path).await), 404);
}

#[tokio::test]
async fn the_documented_endpoints_both_work() {
    let f = start(vec![], 1_000_000).await;
    for path in ["/", "/mcp"] {
        let request = format!(
            "POST {path} HTTP/1.1\r\nContent-Length: {}\r\nAuthorization: Bearer {}\r\n\r\n{PING}",
            PING.len(),
            f.token
        );
        assert_eq!(status(&raw(f.addr, &request).await), 200, "path {path}");
    }
}

/// A notification has no reply; `202` says so rather than returning an empty
/// `200` the client would try to parse.
#[tokio::test]
async fn a_notification_is_accepted_with_no_body_to_parse() {
    let f = start(vec![], 1_000_000).await;
    let note = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    let response = raw(f.addr, &post(Some(&f.token), note, "")).await;
    assert_eq!(status(&response), 202);
}

// ---- resource and smuggling limits -----------------------------------------

#[tokio::test]
async fn an_oversized_body_is_refused_before_it_is_read() {
    let f = start(vec![], 1024).await;
    let big = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"pad":"{}"}}}}"#,
        "A".repeat(100_000)
    );
    let response = raw(f.addr, &post(Some(&f.token), &big, "")).await;
    assert_eq!(status(&response), 413);
}

#[tokio::test]
async fn a_request_without_content_length_is_refused() {
    let f = start(vec![], 1_000_000).await;
    let request = format!(
        "POST / HTTP/1.1\r\nAuthorization: Bearer {}\r\n\r\n{PING}",
        f.token
    );
    assert_eq!(status(&raw(f.addr, &request).await), 411);
}

/// Chunked framing buys nothing for single-message JSON-RPC and is the classic
/// request-smuggling surface. Refusing is safer than implementing.
#[tokio::test]
async fn chunked_encoding_is_refused() {
    let f = start(vec![], 1_000_000).await;
    let request = format!(
        "POST / HTTP/1.1\r\nAuthorization: Bearer {}\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
        f.token
    );
    assert_eq!(status(&raw(f.addr, &request).await), 411);
}

/// Two `Content-Length` headers let a proxy and an origin disagree about where
/// one request ends and the next begins. Refuse the whole message.
#[tokio::test]
async fn a_duplicated_content_length_is_refused() {
    let f = start(vec![], 1_000_000).await;
    let request = format!(
        "POST / HTTP/1.1\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\nContent-Length: 9\r\n\r\n{PING}",
        f.token,
        PING.len()
    );
    assert_eq!(status(&raw(f.addr, &request).await), 400);
}

#[tokio::test]
async fn an_enormous_header_block_is_refused() {
    let f = start(vec![], 1_000_000).await;
    let padding = "X-Pad: ".to_string() + &"a".repeat(64 * 1024) + "\r\n";
    let request = post(Some(&f.token), PING, &padding);
    assert_eq!(status(&raw(f.addr, &request).await), 431);
}

// ---- the property that matters most ----------------------------------------

/// A second transport must not become a second way past the policy gate. The
/// HTTP layer authenticates the caller and then hands the very same
/// `handle_line` the same bytes stdio would have.
#[tokio::test]
async fn the_policy_gate_still_applies_over_http() {
    let f = start(vec![], 1_000_000).await;

    // A tool in a category that is not enabled is refused, exactly as on stdio.
    let call = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"terminal_blocked","arguments":{}}}"#;
    let response = raw(f.addr, &post(Some(&f.token), call, "")).await;
    assert_eq!(status(&response), 200, "policy denials are protocol-level");
    let v = body(&response);
    assert_eq!(v["result"]["isError"], true, "denied call must be an error");
    let text = v["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        !text.contains("reached_engine"),
        "the engine must never have run: {text}"
    );

    // ...and a tool in an enabled category still works, so the check above is
    // not passing because nothing works at all.
    let ok = r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"vision_ok","arguments":{}}}"#;
    let v = body(&raw(f.addr, &post(Some(&f.token), ok, "")).await);
    assert_eq!(v["result"]["isError"], false);
    assert!(v["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("reached_engine"));
}

/// `tools/list` over HTTP must show the same category-filtered set as stdio.
#[tokio::test]
async fn tools_list_is_category_filtered_over_http() {
    let f = start(vec![], 1_000_000).await;
    let request = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    let v = body(&raw(f.addr, &post(Some(&f.token), request, "")).await);
    let names: Vec<&str> = v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"vision_ok"));
    assert!(
        !names.contains(&"terminal_blocked"),
        "a disabled category must not be advertised: {names:?}"
    );
}

/// Garbage over HTTP gets the same JSON-RPC parse error stdio would produce —
/// the transport does not invent its own error vocabulary.
#[tokio::test]
async fn malformed_json_gets_a_jsonrpc_parse_error() {
    let f = start(vec![], 1_000_000).await;
    let v = body(&raw(f.addr, &post(Some(&f.token), "{not json", "")).await);
    assert_eq!(v["error"]["code"], -32700);
}

/// The HTTP transport has nowhere to put a server-initiated frame, so a
/// progress token is accepted and ignored rather than refused: a client written
/// for stdio must still work here, it just hears nothing until the call ends.
#[tokio::test]
async fn a_progress_token_is_accepted_and_quietly_dropped() {
    let f = start(Vec::new(), 64 * 1024).await;
    let req = post(
        Some(&f.token),
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"vision_ok","arguments":{},"_meta":{"progressToken":"p1"}}}"#,
        "",
    );
    let resp = raw(f.addr, &req).await;
    assert_eq!(status(&resp), 200, "{resp}");
    assert!(
        !resp.contains("notifications/progress"),
        "there is no channel for them here"
    );
    assert_eq!(body(&resp)["id"], serde_json::json!(1));
}
