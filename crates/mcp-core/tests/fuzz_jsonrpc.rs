//! Deterministic fuzzing of the JSON-RPC line parser and dispatch entry point.
//!
//! `handle_line` is the *only* thing an untrusted client touches before the
//! policy gate runs, so it has to survive arbitrary bytes without panicking and
//! without breaking protocol framing. A panic here is a denial of service on
//! the whole server; a response containing a raw newline silently desynchronises
//! a newline-delimited stream, which is worse — the client keeps reading, just
//! wrongly.
//!
//! This is a self-contained generator rather than `cargo-fuzz`: it needs no
//! nightly toolchain, no libfuzzer, and no second `target/` directory, and it
//! runs in CI on every platform as an ordinary test. The trade-off is no
//! coverage feedback, so the corpus is seeded by hand with the shapes that
//! matter and mutated structurally.

use std::sync::Arc;

use async_trait::async_trait;
use mcp_core::{Registry, Request, Server};
use mcp_policy::{AuditSink, Mode, Policy, PolicyConfig, Redactor};
use mcp_types::{CallCtx, Category, Envelope, ErrorCode, Tier, ToolDescriptor, ToolModule};
use serde_json::{json, Value};

// ---- harness ---------------------------------------------------------------

struct FuzzModule;

#[async_trait]
impl ToolModule for FuzzModule {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        vec![
            ToolDescriptor::new(
                "fuzz_echo",
                Category::Vision,
                Tier::Read,
                "echoes its arguments",
                json!({ "type": "object", "properties": {}, "required": [] }),
            ),
            ToolDescriptor::new(
                "fuzz_fail",
                Category::Vision,
                Tier::Read,
                "always fails",
                json!({ "type": "object", "properties": {}, "required": [] }),
            ),
        ]
    }

    async fn call(&self, name: &str, args: Value, _ctx: &CallCtx) -> Envelope {
        if name == "fuzz_fail" {
            return Envelope::fail(name, ErrorCode::Internal, "as requested");
        }
        Envelope::ok(name, json!({ "echo": args }))
    }
}

fn server() -> Server {
    let registry = Registry::build(vec![Arc::new(FuzzModule)]).unwrap();
    let cfg = PolicyConfig {
        categories: vec![Category::Vision],
        // Autonomous: never block on a human. A high denial budget keeps the
        // kill switch from tripping early and short-circuiting the rest of the
        // run, which would leave most iterations testing nothing.
        mode: Mode::Autonomous,
        max_denials: usize::MAX,
        ..PolicyConfig::default()
    };
    let policy = Arc::new(Policy::new(cfg, AuditSink::memory(), Redactor::empty()));
    Server::new(registry, policy, "fuzz-session")
}

/// xorshift64*. Deterministic and dependency-free: a failure is reproducible
/// from the iteration number printed in the panic message.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }
}

/// Seed corpus: valid traffic, protocol edge cases, and known-nasty JSON.
fn corpus() -> Vec<String> {
    [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"fuzz_echo","arguments":{"a":1}}}"#,
        r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"fuzz_fail","arguments":{}}}"#,
        r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"nope","arguments":{}}}"#,
        r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"fuzz_echo","arguments":[]}}"#,
        r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":9,"method":"does/not/exist"}"#,
        r#"{"jsonrpc":"1.0","id":10,"method":"ping"}"#,
        r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#,
        r#"{"jsonrpc":"2.0","id":{"nested":"object"},"method":"ping"}"#,
        r#"{"jsonrpc":"2.0","id":"string-id","method":"ping"}"#,
        r#"{"jsonrpc":"2.0","id":-0.0,"method":"ping"}"#,
        r#"{"jsonrpc":"2.0","id":18446744073709551615,"method":"ping"}"#,
        r#"{"jsonrpc":"2.0","id":1e308,"method":"ping"}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"fuzz_echo","arguments":{"s":"line\nbreak\r\nand\ttab"}}}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"fuzz_echo","arguments":{"u":"é😀"}}}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":" "}"#,
        r#"[]"#,
        r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#,
        r#"null"#,
        r#"true"#,
        r#"0"#,
        r#""just a string""#,
        r#"{}"#,
        r#"{"#,
        r#""#,
        r#"   "#,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Structural and byte-level mutations. Byte flips alone rarely get past the
/// first parse error, so most of these edit the JSON *shape*.
fn mutate(input: &str, rng: &mut Rng) -> String {
    let mut b = input.as_bytes().to_vec();
    match rng.below(12) {
        0 if !b.is_empty() => {
            let i = rng.below(b.len());
            b[i] = rng.next() as u8;
        }
        1 if !b.is_empty() => {
            b.truncate(rng.below(b.len()));
        }
        2 => {
            let punctuation: &[u8] = b"{}[]\",:\\ \t";
            let i = rng.below(b.len().max(1));
            let c = *rng.pick(punctuation);
            b.insert(i.min(b.len()), c);
        }
        3 if !b.is_empty() => {
            let i = rng.below(b.len());
            b.remove(i);
        }
        4 => {
            // Repeat a chunk: grows nesting and string lengths fast.
            let n = b.len().min(64);
            let chunk = b[..n].to_vec();
            for _ in 0..rng.below(8) {
                b.extend_from_slice(&chunk);
            }
        }
        5 => {
            // Deep nesting, well past serde_json's recursion limit.
            let depth = 1 + rng.below(4096);
            return nested_request(depth);
        }
        6 => {
            // A very long string value.
            let n = rng.below(50_000);
            return format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"fuzz_echo","arguments":{{"s":"{}"}}}}}}"#,
                "A".repeat(n)
            );
        }
        7 => {
            // A pathological number as the correlation id.
            let odd: &[&str] = &[
                "1e400",
                "-1e400",
                "1e-400",
                "NaN",
                "Infinity",
                "-0",
                "99999999999999999999999999999999",
                "0x10",
                "1_000",
            ];
            let n = *rng.pick(odd);
            return format!(r#"{{"jsonrpc":"2.0","id":{n},"method":"ping"}}"#);
        }
        8 => {
            // A stray continuation byte: invalid UTF-8 mid-request.
            let i = rng.below(b.len().max(1));
            let c = 0x80u8 | (rng.next() as u8 & 0x3f);
            b.insert(i.min(b.len()), c);
        }
        9 => {
            // Duplicate keys: last-wins in serde_json, worth pinning.
            let methods: &[&str] = &["ping", "tools/list", "initialize"];
            let m = *rng.pick(methods);
            return format!(
                r#"{{"jsonrpc":"2.0","jsonrpc":"1.0","id":1,"id":2,"method":"ping","method":"{m}"}}"#
            );
        }
        10 => {
            // Splice two corpus entries together.
            let others = corpus();
            let other = rng.pick(&others).clone();
            let cut = rng.below(b.len().max(1)).min(b.len());
            b.truncate(cut);
            b.extend_from_slice(other.as_bytes());
        }
        _ => {
            let n = rng.below(4);
            b.resize(b.len() + n, b' ');
        }
    }
    String::from_utf8_lossy(&b).into_owned()
}

/// A syntactically valid request whose `params` nest `depth` arrays deep.
fn nested_request(depth: usize) -> String {
    let mut s = String::with_capacity(depth * 2 + 48);
    s.push_str(r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":"#);
    for _ in 0..depth {
        s.push('[');
    }
    for _ in 0..depth {
        s.push(']');
    }
    s.push('}');
    s
}

// ---- invariants ------------------------------------------------------------

/// Every invariant that must hold for *any* input, valid or not.
fn check(input: &str, output: Option<&str>, case: u64) {
    // The oracle deserialises with the server's own `Request` type rather than
    // a loose `Value`. That is deliberate: `Value` accepts duplicate keys and
    // silently keeps the last, while the derive rejects them. Judging the
    // server by the looser rule would report its stricter (and safer) behaviour
    // as a bug — see `duplicate_keys_are_rejected_not_last_wins`.
    let parsed: Option<Request> = serde_json::from_str(input.trim()).ok();

    let Some(out) = output else {
        // Silence is only correct for blank input or a well-formed notification.
        if input.trim().is_empty() {
            return;
        }
        let is_notification = parsed
            .as_ref()
            .is_some_and(|r| r.jsonrpc == "2.0" && r.id.is_none());
        assert!(
            is_notification,
            "case {case}: request got no response: {input:?}"
        );
        return;
    };

    // Framing: a newline-delimited protocol cannot survive a newline inside a
    // frame. serde_json escapes them inside strings; this pins that it holds
    // even when the newline came from the client's own arguments.
    assert!(
        !out.contains('\n') && !out.contains('\r'),
        "case {case}: response broke line framing: {out:?} (input {input:?})"
    );

    let v: Value = serde_json::from_str(out)
        .unwrap_or_else(|e| panic!("case {case}: response is not JSON ({e}): {out:?}"));
    assert!(
        v.is_object(),
        "case {case}: response is not an object: {out}"
    );
    assert_eq!(
        v.get("jsonrpc"),
        Some(&json!("2.0")),
        "case {case}: wrong protocol tag: {out}"
    );
    let has_result = v.get("result").is_some();
    let has_error = v.get("error").is_some();
    assert!(
        has_result ^ has_error,
        "case {case}: response must carry exactly one of result/error: {out}"
    );
    if let Some(err) = v.get("error") {
        assert!(
            err.get("code").is_some_and(Value::is_i64),
            "case {case}: error code must be an integer: {out}"
        );
        assert!(
            err.get("message").is_some_and(Value::is_string),
            "case {case}: error message must be a string: {out}"
        );
    }

    // Correlation: a client matches responses to requests by id, so any request
    // the server accepted must get its own id back, unchanged. A request it
    // could not parse has no id to echo, which is why the oracle only asserts
    // this for inputs that deserialise.
    if let Some(req) = parsed {
        if let Some(id) = req.id.filter(|i| !i.is_null()) {
            if usable_id(&id) {
                assert_eq!(
                    v.get("id"),
                    Some(&id),
                    "case {case}: response id does not match request id: {out}"
                );
            } else {
                // An id the server cannot echo back byte-for-byte is refused
                // outright rather than answered with a subtly different number.
                assert_eq!(
                    v["error"]["code"], -32600,
                    "case {case}: an un-echoable id must be an invalid-request error: {out}"
                );
            }
        }
    }
}

/// Mirrors `mcp_core`'s rule: only a string or an integer survives a JSON
/// round-trip unchanged.
fn usable_id(id: &Value) -> bool {
    match id {
        Value::String(_) | Value::Null => true,
        Value::Number(n) => n.is_i64() || n.is_u64(),
        _ => false,
    }
}

// ---- the runs --------------------------------------------------------------

#[tokio::test]
async fn seed_corpus_upholds_every_invariant() {
    let server = server();
    for (i, input) in corpus().into_iter().enumerate() {
        let out = server.handle_line(&input).await;
        check(&input, out.as_deref(), i as u64);
    }
}

/// How many mutations to run. The default keeps a pull request fast; CI raises
/// it on a schedule, where a long run is affordable and a deeper search into
/// the input space is the whole point.
fn iterations() -> u64 {
    std::env::var("AGENTCTL_FUZZ_ITERATIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000)
}

#[tokio::test]
async fn mutated_inputs_never_break_the_protocol() {
    let server = server();
    let seeds = corpus();
    let mut rng = Rng(0x5eed_1234_abcd_ef01);
    let total = iterations();
    eprintln!("fuzzing handle_line for {total} iterations");
    for i in 0..total {
        let base = rng.pick(&seeds).clone();
        // Occasionally stack several mutations to reach shapes one edit cannot.
        let mut input = mutate(&base, &mut rng);
        for _ in 0..rng.below(3) {
            input = mutate(&input, &mut rng);
        }
        let out = server.handle_line(&input).await;
        check(&input, out.as_deref(), i);
    }
}

/// Nesting deeper than the parser's recursion limit must produce a parse error,
/// not a stack overflow — an overflow aborts the process, and no amount of
/// downstream policy can contain that.
#[tokio::test]
async fn deep_nesting_is_rejected_without_overflowing_the_stack() {
    let server = server();
    for depth in [64usize, 128, 129, 1_000, 100_000] {
        let input = nested_request(depth);
        let out = server.handle_line(&input).await.expect("must answer");
        check(&input, Some(&out), depth as u64);
    }
}

/// Raw bytes with no JSON structure at all.
#[tokio::test]
async fn arbitrary_bytes_are_survivable() {
    let server = server();
    let mut rng = Rng(0xdead_beef_0000_0001);
    for i in 0..iterations() / 4 {
        let n = rng.below(256);
        let bytes: Vec<u8> = (0..n).map(|_| rng.next() as u8).collect();
        let input = String::from_utf8_lossy(&bytes).into_owned();
        let out = server.handle_line(&input).await;
        check(&input, out.as_deref(), i);
    }
}

/// Duplicate keys are a parser-differential attack: `Value` (and most JSON
/// libraries) keep the *last* occurrence, so anything sitting in front of this
/// server — a proxy, an audit tee, a policy shim reading the same bytes — can
/// be shown `"method":"ping"` while the server executes `"method":"tools/call"`.
///
/// The derive-based `Request` refuses the whole message instead, which is the
/// behaviour worth pinning: the fuzzer found this divergence, and it is the
/// strict side that is correct.
#[tokio::test]
async fn duplicate_keys_are_rejected_not_last_wins() {
    let server = server();
    for input in [
        r#"{"jsonrpc":"2.0","id":1,"method":"ping","method":"tools/list"}"#,
        r#"{"jsonrpc":"2.0","id":1,"id":2,"method":"ping"}"#,
        r#"{"jsonrpc":"1.0","jsonrpc":"2.0","id":1,"method":"ping"}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"fuzz_echo"},"params":{"name":"fuzz_fail"}}"#,
    ] {
        let out = server.handle_line(input).await.expect("must answer");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["error"]["code"], -32700,
            "duplicate keys must be a parse error, not last-wins: {input}"
        );
    }
}

/// The fuzzer's extended run found this: `serde_json` does not parse every
/// large float back to the same bits it printed, so echoing a float id returns
/// a number the client cannot match to its request. Refusing is the fix — the
/// JSON-RPC spec already discourages fractional ids.
#[tokio::test]
async fn float_ids_are_refused_because_they_do_not_round_trip() {
    let server = server();
    for id in [
        "9.999999999990999e+31",
        "1.5",
        "-0.5",
        "1e308",
        "99999999999999999999999999999999",
        "{\"nested\":\"object\"}",
        "[1,2,3]",
    ] {
        let input = format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"ping"}}"#);
        let out = server.handle_line(&input).await.expect("must answer");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["error"]["code"], -32600,
            "id {id} must be refused rather than echoed inexactly: {out}"
        );
    }
}

/// ...while the ids every real client actually uses keep working, echoed
/// exactly as sent.
#[tokio::test]
async fn integer_and_string_ids_round_trip_exactly() {
    let server = server();
    for id in [
        "1",
        "0",
        "-1",
        "9007199254740993",     // beyond f64's exact-integer range
        "18446744073709551615", // u64::MAX
        "-9223372036854775808", // i64::MIN
        r#""string-id""#,
        r#""""#,    // the empty string is still a string
        r#""9.5""#, // a float *spelled* as a string is fine
    ] {
        let input = format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"ping"}}"#);
        let out = server.handle_line(&input).await.expect("must answer");
        let v: Value = serde_json::from_str(&out).unwrap();
        let expected: Value = serde_json::from_str(id).unwrap();
        assert_eq!(v["id"], expected, "id {id} must round-trip exactly: {out}");
        assert!(v.get("result").is_some(), "id {id} should have been served");
    }
}
