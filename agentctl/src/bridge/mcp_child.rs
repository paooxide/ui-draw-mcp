//! Speaking MCP to a real `agentctl serve` child process.
//!
//! The bridge deliberately does **not** build a `Server` in-process. The point
//! of a reference client is to prove the thing we ship works over the wire an
//! actual client uses: a spawned binary, stdio framing, JSON-RPC, the policy
//! gate in its own process. An in-process shortcut would test the engines and
//! quietly skip the transport.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// How long any one call may take. Generous, because a GUI tool legitimately
/// waits on a human-speed application; bounded, because a hung child would
/// otherwise hang the demo forever.
const CALL_TIMEOUT: Duration = Duration::from_secs(180);

/// What a tool call produced.
pub struct ToolResult {
    /// The tool envelope, parsed out of the MCP text content block.
    pub envelope: Value,
    /// A note standing in for an image block. The base64 is never carried into
    /// the conversation: one screenshot is roughly a megabyte of context, and
    /// the model cannot act on it any better than on the envelope.
    pub image: Option<Value>,
    pub is_error: bool,
    pub latency_ms: u128,
}

pub struct McpChild {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl McpChild {
    /// Spawn `<exe> serve` with piped stdio. Its stderr is inherited so the
    /// operator sees the server's own log next to the bridge's.
    pub async fn spawn(exe: &Path, config: Option<&Path>) -> Result<Self, String> {
        let mut cmd = Command::new(exe);
        cmd.arg("serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            // If the bridge dies, the server it started must not outlive it.
            .kill_on_drop(true);
        if let Some(path) = config {
            cmd.env("AGENTCTL_CONFIG", path);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("could not start {}: {e}", exe.display()))?;
        let stdin = child.stdin.take().ok_or("the child had no stdin")?;
        let stdout = child.stdout.take().ok_or("the child had no stdout")?;
        Ok(McpChild {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 0,
        })
    }

    async fn write_line(&mut self, v: &Value) -> Result<(), String> {
        let line = serde_json::to_string(v).map_err(|e| e.to_string())?;
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("could not write to the server: {e}"))?;
        self.stdin
            .write_all(b"\n")
            .await
            .map_err(|e| format!("could not write to the server: {e}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| format!("could not flush to the server: {e}"))
    }

    /// One request/response round trip.
    ///
    /// Frames that are not the reply — progress notifications, above all — are
    /// skipped rather than treated as the answer.
    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        self.write_line(&json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        }))
        .await?;
        tokio::time::timeout(CALL_TIMEOUT, self.read_reply(id, method))
            .await
            .map_err(|_| {
                format!(
                    "'{method}' did not answer within {}s",
                    CALL_TIMEOUT.as_secs()
                )
            })?
    }

    async fn read_reply(&mut self, id: u64, method: &str) -> Result<Value, String> {
        loop {
            let mut line = String::new();
            let n = self
                .stdout
                .read_line(&mut line)
                .await
                .map_err(|e| format!("could not read from the server: {e}"))?;
            if n == 0 {
                return Err("the server closed its output".into());
            }
            let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            match v.get("id").and_then(Value::as_u64) {
                Some(got) if got == id => {
                    if let Some(err) = v.get("error") {
                        let msg = err
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown");
                        return Err(format!("{method} failed: {msg}"));
                    }
                    return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                }
                // A notification, or a reply we are no longer waiting for.
                _ => continue,
            }
        }
    }

    /// The MCP handshake.
    pub async fn initialize(&mut self) -> Result<Value, String> {
        let result = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": mcp_core::PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "agentctl-bridge",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
            )
            .await?;
        self.write_line(&json!({
            "jsonrpc": "2.0", "method": "notifications/initialized", "params": {}
        }))
        .await?;
        Ok(result)
    }

    /// The tools this server is offering — after its own category filtering,
    /// which is the list a real client would see.
    pub async fn tools_list(&mut self) -> Result<Vec<Value>, String> {
        let result = self.request("tools/list", json!({})).await?;
        Ok(result
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// Call one tool.
    pub async fn call(&mut self, name: &str, args: &Value) -> Result<ToolResult, String> {
        let started = std::time::Instant::now();
        let result = self
            .request("tools/call", json!({ "name": name, "arguments": args }))
            .await?;
        let latency_ms = started.elapsed().as_millis();
        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let mut envelope = Value::Null;
        let mut image = None;
        for block in result
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                    envelope = serde_json::from_str(text)
                        .unwrap_or_else(|_| json!({ "ok": !is_error, "text": text }));
                }
                Some("image") => {
                    let bytes = block
                        .get("data")
                        .and_then(Value::as_str)
                        .map(|d| d.len() * 3 / 4)
                        .unwrap_or(0);
                    image = Some(json!({
                        "omitted": true,
                        "bytes": bytes,
                        "mimeType": block.get("mimeType").cloned().unwrap_or(Value::Null),
                        "note": "the image is available to the operator; act on the envelope"
                    }));
                }
                _ => {}
            }
        }
        Ok(ToolResult {
            envelope,
            image,
            is_error,
            latency_ms,
        })
    }

    /// Close stdin so the server shuts its engines down, then reap it.
    pub async fn shutdown(mut self) {
        drop(self.stdin);
        let _ = tokio::time::timeout(Duration::from_secs(10), self.child.wait()).await;
        let _ = self.child.start_kill();
    }
}
