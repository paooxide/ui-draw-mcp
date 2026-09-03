//! Test scaffolding shared across the workspace: an in-process MCP client, the
//! live-test gates, and a permissive-but-contained policy for suites that need
//! to actually reach an engine.
//!
//! The client speaks the real protocol through [`mcp_core::Server::handle_line`]
//! rather than calling engines directly, so a test exercises the same path an
//! agent would: framing, dispatch, the policy gate, redaction and audit. A test
//! that bypassed the gate would keep passing after the gate broke.
//!
//! There are no fake backends here, by design. Pure logic is tested as free
//! functions; anything that needs an OS is tested against the real one and
//! skipped when the machine cannot provide it.

use std::sync::atomic::{AtomicU64, Ordering};

use mcp_core::Server;
use mcp_policy::{Mode, PolicyConfig};
use mcp_types::{Category, Envelope};
use serde_json::{json, Value};

/// Drives a [`Server`] over the wire format, in this process.
pub struct InProcClient {
    server: Server,
    next_id: AtomicU64,
}

impl InProcClient {
    pub fn new(server: Server) -> Self {
        InProcClient {
            server,
            next_id: AtomicU64::new(1),
        }
    }

    pub fn server(&self) -> &Server {
        &self.server
    }

    /// One request, one response, as JSON. Panics if the server returns nothing
    /// — that only happens for notifications, which have no id to correlate.
    pub async fn raw(&self, method: &str, params: Value) -> Value {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let line = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string();
        let out = self
            .server
            .handle_line(&line)
            .await
            .unwrap_or_else(|| panic!("no response to {method}"));
        serde_json::from_str(&out).expect("response is JSON")
    }

    pub async fn initialize(&self) -> Value {
        let v = self.raw("initialize", json!({})).await;
        v.get("result").cloned().unwrap_or(Value::Null)
    }

    /// The tools the server would advertise, after category filtering.
    pub async fn tools_list(&self) -> Vec<Value> {
        let v = self.raw("tools/list", json!({})).await;
        v.get("result")
            .and_then(|r| r.get("tools"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    }

    /// Call a tool and get its envelope back.
    ///
    /// The envelope travels as text inside the MCP content array, so it is
    /// parsed back out here; an image block, if any, is reattached so callers
    /// can assert on captures.
    pub async fn call(&self, tool: &str, args: Value) -> Envelope {
        let v = self
            .raw("tools/call", json!({"name": tool, "arguments": args}))
            .await;
        if let Some(err) = v.get("error") {
            panic!("protocol error calling {tool}: {err}");
        }
        let content = v
            .get("result")
            .and_then(|r| r.get("content"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let text = content
            .iter()
            .find(|c| c.get("type").and_then(Value::as_str) == Some("text"))
            .and_then(|c| c.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("{}");
        let mut env: Envelope = serde_json::from_str(text)
            .unwrap_or_else(|e| panic!("envelope from {tool} is not JSON: {e}\n{text}"));
        if let Some(img) = content
            .iter()
            .find(|c| c.get("type").and_then(Value::as_str) == Some("image"))
        {
            env.image = Some(mcp_types::ImageContent {
                mime_type: img
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("image/png")
                    .to_string(),
                base64: img
                    .get("data")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            });
        }
        env
    }

    /// Call and require success, returning the data payload.
    pub async fn ok(&self, tool: &str, args: Value) -> Value {
        let env = self.call(tool, args).await;
        assert!(
            env.ok,
            "{tool} failed: {:?}",
            env.error.as_ref().map(|e| &e.message)
        );
        env.data.unwrap_or(Value::Null)
    }
}

/// Live tests drive real browsers, real package managers and the real desktop.
/// They are the point on a developer's machine and an availability gamble on a
/// hosted runner, so CI sets `AGENTCTL_SKIP_LIVE=1`. `=0` forces them on for a
/// job built to provide what they need.
pub fn skip_live() -> bool {
    std::env::var_os("AGENTCTL_SKIP_LIVE").is_some_and(|v| v != "0")
}

/// GUI suites additionally need an explicit opt-in.
///
/// These steal window focus and type into a real application. A plain
/// `cargo test` on a machine someone is using must never do that, so
/// `AGENTCTL_LIVE_GUI=1` is required on top of the live gate.
pub fn live_gui_enabled() -> bool {
    !skip_live() && std::env::var_os("AGENTCTL_LIVE_GUI").is_some_and(|v| v == "1")
}

/// A config for suites that need to reach engines: autonomous (nothing can
/// prompt a human mid-test), no denial cap, and a kill-switch path and audit
/// directory under a per-process temp dir so a test can never trip or read the
/// developer's real ones.
pub fn test_policy(categories: &[Category]) -> PolicyConfig {
    let dir = std::env::temp_dir().join(format!("agentctl-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    PolicyConfig {
        categories: categories.to_vec(),
        mode: Mode::Autonomous,
        max_denials: usize::MAX,
        kill_switch_file: dir.join("STOP"),
        audit_dir: dir.join("audit"),
        ..PolicyConfig::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_test_policy_never_points_at_the_real_kill_switch() {
        let p = test_policy(&[Category::System]);
        let real = PolicyConfig::default().kill_switch_file;
        assert_ne!(p.kill_switch_file, real);
        assert!(p.kill_switch_file.starts_with(std::env::temp_dir()));
    }

    #[test]
    fn gui_tests_need_both_gates() {
        // Whatever the ambient environment is, the GUI gate can never be open
        // while the live gate is closed.
        if skip_live() {
            assert!(!live_gui_enabled());
        }
    }
}
