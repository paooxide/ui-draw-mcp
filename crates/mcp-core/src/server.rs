use std::sync::Arc;

use mcp_policy::{now_ms, AuditRecord, Decision, Policy};
use mcp_types::{CallCtx, CancelToken, Envelope, ErrorCode};
use serde::Serialize;
use serde_json::{json, Value};

use crate::jsonrpc::{
    Request, Response, INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND, PARSE_ERROR,
};
use crate::registry::Registry;

/// The MCP protocol revision this server implements (confirmed current stable).
pub const PROTOCOL_VERSION: &str = "2025-11-25";

/// Default cap on a single JSON-RPC frame. Large enough for a realistic
/// `fs_write` payload, small enough that an unterminated frame cannot exhaust
/// memory. Override with [`Server::with_max_frame_bytes`].
pub const DEFAULT_MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// The running server: a tool registry + the policy kernel + a session id.
pub struct Server {
    registry: Registry,
    policy: Arc<Policy>,
    session_id: String,
    max_frame_bytes: usize,
}

impl Server {
    pub fn new(registry: Registry, policy: Arc<Policy>, session_id: impl Into<String>) -> Self {
        Server {
            registry,
            policy,
            session_id: session_id.into(),
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
        }
    }

    /// Cap the size of a single JSON-RPC frame.
    pub fn with_max_frame_bytes(mut self, max: usize) -> Self {
        self.max_frame_bytes = max;
        self
    }

    pub fn max_frame_bytes(&self) -> usize {
        self.max_frame_bytes
    }

    pub fn policy(&self) -> &Arc<Policy> {
        &self.policy
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Stop serving: let every engine release child processes, temporary
    /// directories and sessions. Idempotent, and safe to call from any exit
    /// path (EOF, transport error, signal).
    pub fn shutdown(&self) {
        self.registry.shutdown_all();
    }

    // ---- dispatch (the single security-relevant path; arch §2.3, §6) --------

    /// Validate → gate → audit → execute → redact → audit. The only path that
    /// reaches an engine.
    pub async fn dispatch_call(&self, name: &str, args: Value) -> Envelope {
        // 1. Kill switch first.
        if self.policy.kill_switch_tripped() {
            let mut pre = AuditRecord::pre(&self.session_id, name);
            pre.decision = Some("kill_switch".into());
            self.policy.audit(&pre);
            return Envelope::fail(name, ErrorCode::Timeout, "kill switch engaged");
        }

        // 2. Look up the tool.
        let Some((module, descriptor)) = self.registry.find(name) else {
            let mut pre = AuditRecord::pre(&self.session_id, name);
            pre.decision = Some("unknown_tool".into());
            self.policy.audit(&pre);
            return Envelope::fail_with(
                name,
                ErrorCode::InvalidArgs,
                "unknown tool",
                "call tools/list to see available tools",
            );
        };
        let tier = enum_str(&descriptor.tier);

        // 3. Shallow arg-shape check (deep validation is the engine's job).
        if !(args.is_object() || args.is_null()) {
            return Envelope::fail(
                name,
                ErrorCode::InvalidArgs,
                "arguments must be a JSON object",
            );
        }

        // 4. Gate (category -> tier). An engine may additionally flag *this*
        //    invocation as risky (e.g. a destructive command), which upgrades an
        //    otherwise-allowed call to needing human approval.
        let mut decision = self.policy.gate(descriptor);
        if decision.is_allow() {
            if let Some(prompt) = module.consent_prompt(name, &args) {
                decision = Decision::NeedConsent { prompt };
            }
        }
        let decision = self.policy.resolve_consent(decision);

        // Pre-audit with redacted args.
        let mut pre = AuditRecord::pre(&self.session_id, name);
        pre.tier = Some(tier);
        let mut redacted_args = args.clone();
        self.policy.redact(&mut redacted_args);
        pre.args_redacted = Some(redacted_args);

        match &decision {
            Decision::Deny { code, reason } => {
                pre.decision = Some("deny".into());
                self.policy.audit(&pre);
                self.policy.record_denial();
                return Envelope::fail(name, *code, reason.clone());
            }
            Decision::NeedConsent { prompt } => {
                // Ask a human out of band. Only the core may do this; engines
                // merely describe risk, so nothing here can approve itself.
                let outcome = self.policy.request_consent(
                    &self.session_id,
                    name,
                    prompt,
                    pre.args_redacted.as_ref().map(|a| a.to_string()),
                );
                pre.decision = Some(format!("consent:{outcome:?}").to_lowercase());
                self.policy.audit(&pre);
                if !outcome.approved() {
                    self.policy.record_denial();
                    return Envelope::fail(
                        name,
                        ErrorCode::ConsentRequired,
                        format!("{prompt} — {}", outcome.reason()),
                    );
                }
            }
            Decision::Allow => {
                pre.decision = Some("allow".into());
                self.policy.audit(&pre);
            }
        }

        // 5. Execute (descriptor borrow has ended; `module` is an owned Arc).
        let start = now_ms();
        let ctx = CallCtx::new(self.session_id.clone(), CancelToken::new());
        let result = module.call(name, args, &ctx).await;

        // 6. Redact result.
        let result = self.policy.redact_envelope(result);

        // 7. Post-audit.
        let mut post = AuditRecord::post(&self.session_id, name);
        post.ok = Some(result.ok);
        post.error_code = result.error.as_ref().map(|e| enum_str(&e.code));
        post.latency_ms = Some(now_ms().saturating_sub(start));
        self.policy.audit(&post);

        result
    }

    // ---- protocol -----------------------------------------------------------

    /// Descriptors visible to the agent = those in an enabled category.
    fn visible_descriptors(&self) -> Vec<Value> {
        self.registry
            .descriptors()
            .filter(|d| self.policy.is_category_enabled(d.category))
            .map(|d| {
                json!({
                    "name": d.name,
                    "description": d.description,
                    "inputSchema": d.input_schema,
                    "x-tier": enum_str(&d.tier),
                    "x-category": d.category.slug(),
                })
            })
            .collect()
    }

    fn initialize_result(&self) -> Value {
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": { "listChanged": true } },
            "serverInfo": { "name": "agentctl", "version": env!("CARGO_PKG_VERSION") },
            "instructions": "Tools are tiered read/standard/dangerous and grouped by \
                category. Only enabled categories are listed. Dangerous tools require \
                opt-in and may require human consent. Every call is audited; a kill \
                switch can abort activity."
        })
    }

    fn tool_call_result(&self, mut env: Envelope) -> Value {
        let is_error = !env.ok;
        // Emit any image as its own MCP content block; keep it out of the text
        // block so the base64 isn't duplicated.
        let image = env.image.take();
        let text = serde_json::to_string(&env).unwrap_or_else(|_| "{}".to_string());
        let mut content = vec![json!({ "type": "text", "text": text })];
        if let Some(img) = image {
            content.push(json!({ "type": "image", "data": img.base64, "mimeType": img.mime_type }));
        }
        json!({ "content": content, "isError": is_error })
    }

    /// Route one parsed request. Returns `None` for notifications (no `id`).
    pub async fn handle_request(&self, req: Request) -> Option<Response> {
        let is_notification = req.id.is_none();
        let id = req.id.clone();

        let response = match req.method.as_str() {
            "initialize" => Response::success(id, self.initialize_result()),
            // Normally a notification, and swallowed by the check below. But
            // "is this a notification?" is answered by the *absence of an id*,
            // not by the method name — a client that sends this with an id is
            // making a request, and returning nothing would leave it waiting
            // forever. Found by the fuzzer's extended run.
            "notifications/initialized" => Response::success(id, json!({})),
            "ping" => Response::success(id, json!({})),
            "tools/list" => Response::success(id, json!({ "tools": self.visible_descriptors() })),
            "tools/call" => {
                let params = req.params.unwrap_or(Value::Null);
                let Some(name) = params.get("name").and_then(|v| v.as_str()) else {
                    return Some(Response::error(id, INVALID_PARAMS, "missing tool name"));
                };
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let env = self.dispatch_call(name, args).await;
                Response::success(id, self.tool_call_result(env))
            }
            other => Response::error(id, METHOD_NOT_FOUND, format!("method not found: {other}")),
        };

        if is_notification {
            None
        } else {
            Some(response)
        }
    }

    /// Parse one line of input and produce the serialized response line, if any.
    pub async fn handle_line(&self, line: &str) -> Option<String> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }
        let req: Request = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                let resp = Response::error(None, PARSE_ERROR, format!("parse error: {e}"));
                return Some(serialize(&resp));
            }
        };
        if req.jsonrpc != "2.0" {
            let resp = Response::error(
                req.id,
                INVALID_REQUEST,
                "invalid request: jsonrpc must be \"2.0\"",
            );
            return Some(serialize(&resp));
        }
        if let Some(id) = &req.id {
            if !is_usable_id(id) {
                // Echoed with a null id: we cannot correlate a response to an
                // id we are refusing to handle.
                return Some(serialize(&Response::error(
                    None,
                    INVALID_REQUEST,
                    "invalid request: id must be a string or an integer",
                )));
            }
        }
        let resp = self.handle_request(req).await?;
        Some(serialize(&resp))
    }

    /// Serve over stdio: read newline-delimited JSON-RPC, write responses to
    /// stdout (which is protocol-only; logs go to stderr).
    pub async fn serve_stdio(&self) -> std::io::Result<()> {
        use tokio::io::BufReader;
        self.serve_stream(BufReader::new(tokio::io::stdin()), tokio::io::stdout())
            .await
    }

    /// The transport loop, over any reader/writer pair. Split out from
    /// [`Self::serve_stdio`] so it can be driven by a test — and by the HTTP
    /// transport — without a real terminal.
    pub async fn serve_stream<R, W>(&self, mut reader: R, mut writer: W) -> std::io::Result<()>
    where
        R: tokio::io::AsyncBufRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        use tokio::io::AsyncWriteExt;

        loop {
            let frame = read_frame(&mut reader, self.max_frame_bytes).await?;
            let response = match frame {
                Frame::Eof => return Ok(()),
                Frame::Line(line) => self.handle_line(&line).await,
                // Answer rather than disconnect: the oversized frame has been
                // drained, so the stream is back in sync at the next newline
                // and a well-behaved client can carry on.
                Frame::TooLong(len) => Some(serialize(&Response::error(
                    None,
                    INVALID_REQUEST,
                    format!(
                        "request frame of {len} bytes exceeds the {} byte limit",
                        self.max_frame_bytes
                    ),
                ))),
            };
            if let Some(resp) = response {
                writer.write_all(resp.as_bytes()).await?;
                writer.write_all(b"\n").await?;
                writer.flush().await?;
            }
        }
    }
}

/// One frame read off the wire.
enum Frame {
    Eof,
    Line(String),
    /// The frame exceeded the cap and was discarded; carries its size.
    TooLong(usize),
}

/// Read one newline-delimited frame, refusing to buffer more than `max` bytes.
///
/// `BufReader::lines()` grows its buffer without bound, so a client that opens
/// a frame and never closes it can drive the process out of memory *before any
/// policy runs* — the cheapest denial of service against this server there is.
/// Past the cap the remaining bytes are consumed and dropped rather than
/// accumulated, so memory stays flat and the stream resynchronises at the next
/// newline instead of the session dying.
async fn read_frame<R>(reader: &mut R, max: usize) -> std::io::Result<Frame>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;

    let mut buf: Vec<u8> = Vec::new();
    // Non-zero once the cap is passed: total bytes seen, with `buf` released.
    let mut discarded = 0usize;

    loop {
        let (consumed, complete) = {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                return Ok(match (discarded, buf.is_empty()) {
                    (0, true) => Frame::Eof,
                    (0, false) => Frame::Line(String::from_utf8_lossy(&buf).into_owned()),
                    _ => Frame::TooLong(discarded),
                });
            }
            match available.iter().position(|b| *b == b'\n') {
                Some(at) => {
                    if discarded == 0 {
                        buf.extend_from_slice(&available[..at]);
                    }
                    (at + 1, true)
                }
                None => {
                    let n = available.len();
                    if discarded == 0 {
                        buf.extend_from_slice(available);
                    }
                    (n, false)
                }
            }
        };
        reader.consume(consumed);

        if discarded > 0 {
            discarded = discarded.saturating_add(consumed);
        } else if buf.len() > max {
            discarded = buf.len();
            buf = Vec::new();
            buf.shrink_to_fit();
        }

        if complete {
            return Ok(if discarded > 0 {
                Frame::TooLong(discarded)
            } else {
                Frame::Line(String::from_utf8_lossy(&buf).into_owned())
            });
        }
    }
}

/// Serialize a response, falling back to a hand-written internal error rather
/// than an empty line: a blank frame would leave the client waiting on a reply
/// that already happened.
fn serialize(resp: &Response) -> String {
    serde_json::to_string(resp).unwrap_or_else(|_| {
        r#"{"jsonrpc":"2.0","error":{"code":-32603,"message":"response serialization failed"}}"#
            .to_string()
    })
}

/// Is this `id` one we can echo back unchanged?
///
/// A client matches a response to its request by comparing ids, so an id that
/// does not survive a parse/serialize round-trip is worse than useless — it
/// looks like a reply to a request that was never made. Floating-point is
/// exactly that case: `serde_json`'s parser is not correctly rounded at large
/// magnitudes, so `9.999999999990999e+31` comes back as a *different* number.
///
/// JSON-RPC 2.0 already says an id SHOULD be a String, Number or Null and that
/// numbers "SHOULD NOT contain fractional parts", so refusing anything but a
/// string or an integer costs no real client anything. Found by the fuzzer's
/// extended run; pinned by `float_ids_are_refused_because_they_do_not_round_trip`.
fn is_usable_id(id: &Value) -> bool {
    match id {
        Value::String(_) | Value::Null => true,
        Value::Number(n) => n.is_i64() || n.is_u64(),
        // Objects and arrays are out of spec, and a nested float would have the
        // same round-trip problem.
        _ => false,
    }
}

/// Serialize a simple serde enum to its string form (e.g. `Tier::Standard` ->
/// `"standard"`, `ErrorCode::PolicyDenied` -> `"POLICY_DENIED"`).
fn enum_str<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}
