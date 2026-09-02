use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use mcp_types::{CallCtx, Category, Envelope, ErrorCode, Tier, ToolDescriptor, ToolModule};
use serde_json::{json, Value};

/// The `system` engine: diagnostic tools that exercise the full dispatch path
/// (`ping`, `echo`). This is the walking-skeleton engine; real capability
/// engines follow the same `ToolModule` contract.
pub struct SystemModule;

#[async_trait]
impl ToolModule for SystemModule {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        vec![
            ToolDescriptor::new(
                "ping",
                Category::System,
                Tier::Read,
                "Health check. Returns pong and the server time in milliseconds.",
                json!({ "type": "object", "properties": {}, "required": [] }),
            ),
            ToolDescriptor::new(
                "echo",
                Category::System,
                Tier::Read,
                "Echo back the provided message. Useful for connectivity checks.",
                json!({
                    "type": "object",
                    "properties": {
                        "message": { "type": "string", "description": "text to echo back" }
                    },
                    "required": ["message"]
                }),
            ),
        ]
    }

    async fn call(&self, name: &str, args: Value, _ctx: &CallCtx) -> Envelope {
        match name {
            "ping" => Envelope::ok("ping", json!({ "pong": true, "ts_ms": now_ms() })),
            "echo" => match args.get("message").and_then(Value::as_str) {
                Some(message) => Envelope::ok("echo", json!({ "message": message })),
                None => Envelope::fail(
                    "echo",
                    ErrorCode::InvalidArgs,
                    "missing required string field 'message'",
                ),
            },
            other => Envelope::fail(other, ErrorCode::InvalidArgs, "unknown tool"),
        }
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_types::CancelToken;

    fn ctx() -> CallCtx {
        CallCtx::new("test", CancelToken::new())
    }

    #[tokio::test]
    async fn ping_returns_pong() {
        let env = SystemModule.call("ping", json!({}), &ctx()).await;
        assert!(env.ok);
        assert_eq!(env.data.unwrap()["pong"], true);
    }

    #[tokio::test]
    async fn echo_round_trips_message() {
        let env = SystemModule
            .call("echo", json!({ "message": "hi" }), &ctx())
            .await;
        assert_eq!(env.data.unwrap()["message"], "hi");
    }

    #[tokio::test]
    async fn echo_without_message_is_invalid_args() {
        let env = SystemModule.call("echo", json!({}), &ctx()).await;
        assert!(!env.ok);
        assert_eq!(env.error.unwrap().code, ErrorCode::InvalidArgs);
    }
}
