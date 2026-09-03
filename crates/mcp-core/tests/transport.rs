//! Transport-level tests for the newline-delimited framing loop.
//!
//! `handle_line` is covered by `fuzz_jsonrpc.rs`; this file covers the layer
//! *below* it — how bytes become frames. The failure mode that matters here is
//! resource exhaustion before any policy runs: a client that opens a frame and
//! never closes it, which `BufReader::lines()` would happily buffer until the
//! process dies.

use std::sync::Arc;

use async_trait::async_trait;
use mcp_core::{Registry, Server};
use mcp_policy::{AuditSink, Mode, Policy, PolicyConfig, Redactor};
use mcp_types::{CallCtx, Category, Envelope, Tier, ToolDescriptor, ToolModule};
use serde_json::{json, Value};
use tokio::io::BufReader;

struct EchoModule;

#[async_trait]
impl ToolModule for EchoModule {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        vec![ToolDescriptor::new(
            "echo_tool",
            Category::Vision,
            Tier::Read,
            "echoes its arguments",
            json!({ "type": "object", "properties": {}, "required": [] }),
        )]
    }

    async fn call(&self, name: &str, args: Value, _ctx: &CallCtx) -> Envelope {
        Envelope::ok(name, json!({ "echo": args }))
    }
}

fn server() -> Server {
    let registry = Registry::build(vec![Arc::new(EchoModule)]).unwrap();
    let cfg = PolicyConfig {
        categories: vec![Category::Vision],
        mode: Mode::Autonomous,
        max_denials: usize::MAX,
        ..PolicyConfig::default()
    };
    let policy = Arc::new(Policy::new(cfg, AuditSink::memory(), Redactor::empty()));
    Server::new(registry, policy, "transport-session")
}

/// Drive the framing loop over an in-memory stream and return one parsed
/// response per output line.
async fn exchange(server: &Server, input: &str) -> Vec<Value> {
    let mut out: Vec<u8> = Vec::new();
    server
        .serve_stream(BufReader::new(input.as_bytes()), &mut out)
        .await
        .expect("stream must drain cleanly");
    String::from_utf8(out)
        .expect("responses must be valid UTF-8")
        .lines()
        .map(|l| serde_json::from_str(l).expect("each output line must be one JSON value"))
        .collect()
}

#[tokio::test]
async fn frames_are_answered_one_per_line_in_order() {
    let server = server();
    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#,
        "\n",
    );
    let responses = exchange(&server, input).await;
    assert_eq!(responses.len(), 3);
    assert_eq!(responses[0]["id"], 1);
    assert_eq!(responses[1]["id"], 2);
    assert_eq!(responses[2]["id"], 3);
}

/// Notifications are silent, and their silence must not shift the pairing of
/// later responses to requests.
#[tokio::test]
async fn notifications_are_silent_without_desynchronising() {
    let server = server();
    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#,
        "\n",
    );
    let responses = exchange(&server, input).await;
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0]["id"], 1);
    assert_eq!(responses[1]["id"], 2);
}

/// Blank lines are ignored rather than answered with a parse error — clients
/// and shells both produce stray newlines.
#[tokio::test]
async fn blank_lines_are_ignored() {
    let server = server();
    let input = concat!(
        "\n",
        "   \n",
        r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
        "\n",
        "\n",
    );
    let responses = exchange(&server, input).await;
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["id"], 1);
}

/// A final frame with no trailing newline is still a frame. Dropping it would
/// lose the last request of any client that does not terminate its output.
#[tokio::test]
async fn final_frame_without_a_newline_is_processed() {
    let server = server();
    let responses = exchange(&server, r#"{"jsonrpc":"2.0","id":9,"method":"ping"}"#).await;
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["id"], 9);
}

/// The headline case: an oversized frame must be refused *and* the stream must
/// keep working. Answering and resynchronising beats disconnecting, because a
/// client that overran once is usually still healthy.
#[tokio::test]
async fn oversized_frame_is_refused_and_the_stream_resynchronises() {
    let server = server().with_max_frame_bytes(4096);
    let giant = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"echo_tool","arguments":{{"s":"{}"}}}}}}"#,
        "A".repeat(200_000)
    );
    let input = format!(
        "{giant}\n{}\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#
    );
    let responses = exchange(&server, &input).await;

    assert_eq!(responses.len(), 2, "both frames must be answered");
    assert_eq!(
        responses[0]["error"]["code"], -32600,
        "the oversized frame must be an invalid-request error"
    );
    assert!(
        responses[0]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("exceeds"),
        "the error must say what happened: {}",
        responses[0]["error"]["message"]
    );
    assert_eq!(
        responses[1]["id"], 2,
        "the stream must resynchronise at the next newline"
    );
}

/// An unterminated frame — bytes with no newline, ever — is the actual denial
/// of service. It must end at EOF having refused, not having buffered it all.
#[tokio::test]
async fn unterminated_oversized_frame_ends_at_eof_without_buffering() {
    let server = server().with_max_frame_bytes(1024);
    let input = "B".repeat(5_000_000);
    let responses = exchange(&server, &input).await;
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["error"]["code"], -32600);
}

/// A frame right at the cap is still valid: the limit must be inclusive of
/// legitimate payloads, not off by enough to reject them.
#[tokio::test]
async fn a_frame_just_under_the_cap_is_accepted() {
    let server = server().with_max_frame_bytes(4096);
    let filler = 4096 - 100;
    let request = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"echo_tool","arguments":{{"s":"{}"}}}}}}"#,
        "A".repeat(filler)
    );
    assert!(request.len() <= 4096, "fixture must fit under the cap");
    let responses = exchange(&server, &format!("{request}\n")).await;
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["id"], 1);
    assert_eq!(responses[0]["result"]["isError"], false);
}

/// The default cap is generous enough for real payloads but finite.
#[tokio::test]
async fn the_default_cap_is_finite_and_roomy() {
    let cap = server().max_frame_bytes();
    assert_eq!(cap, mcp_core::DEFAULT_MAX_FRAME_BYTES);
    assert!(cap >= 1024 * 1024, "too small for a realistic fs_write");
    assert!(
        cap <= 64 * 1024 * 1024,
        "large enough to be a memory problem"
    );
}

/// Whether a message is a notification is decided by the absence of an `id`,
/// never by the method name. A client that puts an id on a notification method
/// is making a request; answering nothing would hang it. Found by the fuzzer.
#[tokio::test]
async fn a_notification_method_sent_with_an_id_still_gets_a_reply() {
    let server = server();
    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"notifications/initialized"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#,
        "\n",
    );
    let responses = exchange(&server, input).await;
    assert_eq!(
        responses.len(),
        2,
        "the one with an id must be answered; the one without must not"
    );
    assert_eq!(responses[0]["id"], 1);
    assert!(responses[0]["result"].is_object());
    assert_eq!(responses[1]["id"], 2);
}

/// A slow call must not be silence.
///
/// Progress frames have to arrive *while* the call is running, not batched
/// after it — batching them would defeat the point — and they must always
/// precede the response they belong to, so a client can attribute them.
#[tokio::test]
async fn progress_frames_arrive_before_the_response() {
    struct Slow;
    #[async_trait::async_trait]
    impl ToolModule for Slow {
        fn descriptors(&self) -> Vec<ToolDescriptor> {
            vec![ToolDescriptor::new(
                "slow",
                Category::System,
                Tier::Read,
                "reports progress",
                json!({"type":"object","properties":{},"required":[]}),
            )]
        }
        async fn call(&self, name: &str, _a: Value, ctx: &CallCtx) -> Envelope {
            for i in 1..=3 {
                ctx.progress(f64::from(i), Some(3.0), Some("step"));
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            Envelope::ok(name, json!({"done": true}))
        }
    }
    let registry = Registry::build(vec![Arc::new(Slow)]).unwrap();
    let policy = Arc::new(Policy::new(
        PolicyConfig {
            categories: vec![Category::System],
            ..PolicyConfig::default()
        },
        AuditSink::memory(),
        Redactor::empty(),
    ));
    let server = Server::new(registry, policy, "progress-test");

    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"slow","arguments":{},"_meta":{"progressToken":"p1"}}}"#,
        "\n"
    );
    let out = exchange(&server, input).await;
    // Three notifications, then the response.
    assert_eq!(
        out.len(),
        4,
        "expected 3 progress frames + 1 response: {out:?}"
    );
    for (i, frame) in out.iter().take(3).enumerate() {
        assert_eq!(frame["method"], json!("notifications/progress"));
        assert_eq!(frame["params"]["progressToken"], json!("p1"));
        assert_eq!(frame["params"]["progress"], json!(i as f64 + 1.0));
        assert_eq!(frame["params"]["total"], json!(3.0));
        assert!(frame.get("id").is_none(), "a notification has no id");
    }
    assert_eq!(out[3]["id"], json!(1));
    assert!(out[3]["result"].is_object());
}

/// Without a token there is nothing to correlate a report to, so none are sent.
#[tokio::test]
async fn no_progress_token_means_no_notifications() {
    struct Chatty;
    #[async_trait::async_trait]
    impl ToolModule for Chatty {
        fn descriptors(&self) -> Vec<ToolDescriptor> {
            vec![ToolDescriptor::new(
                "chatty",
                Category::System,
                Tier::Read,
                "reports progress",
                json!({"type":"object","properties":{},"required":[]}),
            )]
        }
        async fn call(&self, name: &str, _a: Value, ctx: &CallCtx) -> Envelope {
            ctx.progress(1.0, None, Some("hello"));
            Envelope::ok(name, json!({}))
        }
    }
    let registry = Registry::build(vec![Arc::new(Chatty)]).unwrap();
    let policy = Arc::new(Policy::new(
        PolicyConfig {
            categories: vec![Category::System],
            ..PolicyConfig::default()
        },
        AuditSink::memory(),
        Redactor::empty(),
    ));
    let server = Server::new(registry, policy, "quiet-test");
    let out = exchange(
        &server,
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"chatty\",\"arguments\":{}}}\n",
    )
    .await;
    assert_eq!(out.len(), 1, "only the response: {out:?}");
    assert_eq!(out[0]["id"], json!(1));
}

/// A float token cannot survive the round trip intact, so a client could never
/// match the report to its call — the same reason request ids refuse them.
#[tokio::test]
async fn a_non_integer_progress_token_is_ignored() {
    let server = server();
    let out = exchange(
        &server,
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"echo\",\"arguments\":{},\"_meta\":{\"progressToken\":1.5}}}\n",
    )
    .await;
    assert!(out.iter().all(|f| f.get("method").is_none()));
}
