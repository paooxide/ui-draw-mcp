use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Stable error codes returned to the agent. Every engine maps native failures
/// to exactly one of these; `Internal` is logged in full but returned generic
/// (see `docs/architecture.md` §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    /// The OS refused the action (missing permission, etc.).
    PermDenied,
    /// Our policy layer refused the action (category/tier/allowlist).
    PolicyDenied,
    /// A dangerous action needs human consent that was not granted.
    ConsentRequired,
    /// A referenced element/app/window/session was not found.
    NotFound,
    /// An element ref is no longer valid for the current snapshot.
    StaleRef,
    /// The action exceeded its time budget (or the kill switch fired).
    Timeout,
    /// Arguments failed schema or semantic validation.
    InvalidArgs,
    /// The capability is not supported on this OS.
    UnsupportedOs,
    /// The action was attempted but failed at the OS layer.
    ActionFailed,
    /// An internal bug in the server (never leaks detail to the agent).
    Internal,
}

/// The error half of an [`Envelope`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

/// An image result (base64), surfaced as an MCP `image` content block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageContent {
    pub mime_type: String,
    pub base64: String,
}

/// The universal result of any tool call. Same shape for success and failure so
/// the agent can branch on `ok`/`error.code`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub ok: bool,
    pub tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ToolError>,
    /// Optional image payload (capture tools). Rendered as an MCP image block.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageContent>,
}

impl Envelope {
    /// A successful result carrying `data`.
    pub fn ok(tool: impl Into<String>, data: Value) -> Self {
        Envelope {
            ok: true,
            tool: tool.into(),
            data: Some(data),
            error: None,
            image: None,
        }
    }

    /// A successful result carrying `data` and an image (capture tools).
    pub fn ok_image(tool: impl Into<String>, data: Value, image: ImageContent) -> Self {
        Envelope {
            ok: true,
            tool: tool.into(),
            data: Some(data),
            error: None,
            image: Some(image),
        }
    }

    /// A failure with a code and message.
    pub fn fail(tool: impl Into<String>, code: ErrorCode, message: impl Into<String>) -> Self {
        Envelope {
            ok: false,
            tool: tool.into(),
            data: None,
            error: Some(ToolError {
                code,
                message: message.into(),
                suggestion: None,
            }),
            image: None,
        }
    }

    /// A failure with a recovery suggestion for the agent.
    pub fn fail_with(
        tool: impl Into<String>,
        code: ErrorCode,
        message: impl Into<String>,
        suggestion: impl Into<String>,
    ) -> Self {
        Envelope {
            ok: false,
            tool: tool.into(),
            data: None,
            error: Some(ToolError {
                code,
                message: message.into(),
                suggestion: Some(suggestion.into()),
            }),
            image: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_code_serializes_screaming_snake() {
        let j = serde_json::to_string(&ErrorCode::PolicyDenied).unwrap();
        assert_eq!(j, "\"POLICY_DENIED\"");
        let j = serde_json::to_string(&ErrorCode::StaleRef).unwrap();
        assert_eq!(j, "\"STALE_REF\"");
    }

    #[test]
    fn ok_envelope_omits_error_field() {
        let e = Envelope::ok("ping", serde_json::json!({"pong": true}));
        let j = serde_json::to_value(&e).unwrap();
        assert_eq!(j["ok"], true);
        assert!(j.get("error").is_none());
        assert_eq!(j["data"]["pong"], true);
    }

    #[test]
    fn fail_envelope_omits_data_field() {
        let e = Envelope::fail("click", ErrorCode::NotFound, "no such element");
        let j = serde_json::to_value(&e).unwrap();
        assert_eq!(j["ok"], false);
        assert!(j.get("data").is_none());
        assert_eq!(j["error"]["code"], "NOT_FOUND");
    }

    #[test]
    fn envelope_round_trips() {
        let e = Envelope::fail_with("x", ErrorCode::Timeout, "slow", "retry");
        let s = serde_json::to_string(&e).unwrap();
        let back: Envelope = serde_json::from_str(&s).unwrap();
        assert!(!back.ok);
        assert_eq!(back.error.unwrap().suggestion.as_deref(), Some("retry"));
    }
}
