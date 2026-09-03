//! A record of one agent session, in a shape both demo routes can produce.
//!
//! There are two ways to drive this server with a real model: the Gemini
//! bridge, which controls the whole loop, and a client like Claude Code, which
//! does not cooperate with us at all. The second one can still be recorded,
//! because everything a tool call did is already in the audit log — that is
//! what the log is for. One schema, two producers, one test that checks either.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Versioned so a fixture recorded today still identifies itself later.
pub const SCHEMA: &str = "agentctl-demo-transcript/1";

/// How much of a result is kept. Enough to see what came back, not enough to
/// turn a transcript into a copy of someone's screen.
const EXCERPT_CHARS: usize = 200;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transcript {
    pub schema: String,
    /// What drove the session: `gemini-bridge`, `claude-code`, …
    pub client: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub task: String,
    pub started_ms: u128,
    pub turns: Vec<Turn>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    /// `user`, `model` or `tool`.
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    /// What the policy decided, when it is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<ResultSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultSummary {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    pub latency_ms: u128,
    pub excerpt: String,
}

impl Transcript {
    pub fn new(client: impl Into<String>, model: Option<String>, task: impl Into<String>) -> Self {
        Transcript {
            schema: SCHEMA.to_string(),
            client: client.into(),
            model,
            task: task.into(),
            started_ms: mcp_policy::now_ms(),
            turns: Vec::new(),
        }
    }

    pub fn say(&mut self, role: &str, text: impl Into<String>) {
        self.turns.push(Turn {
            role: role.to_string(),
            text: Some(text.into()),
            tool: None,
            args: None,
            decision: None,
            result: None,
        });
    }

    pub fn tool_call(&mut self, tool: &str, args: Value, result: ResultSummary) {
        self.turns.push(Turn {
            role: "tool".to_string(),
            text: None,
            tool: Some(tool.to_string()),
            args: Some(args),
            decision: None,
            result: Some(result),
        });
    }

    /// How many tool calls succeeded, and how many were attempted.
    pub fn tally(&self) -> (usize, usize) {
        let calls: Vec<&Turn> = self.turns.iter().filter(|t| t.tool.is_some()).collect();
        let ok = calls
            .iter()
            .filter(|t| t.result.as_ref().is_some_and(|r| r.ok))
            .count();
        (ok, calls.len())
    }

    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// A short, safe rendering of a result value.
pub fn excerpt(v: &Value) -> String {
    let s = match v {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    };
    let s = s.replace('\n', " ");
    if s.chars().count() <= EXCERPT_CHARS {
        return s;
    }
    let cut: String = s.chars().take(EXCERPT_CHARS).collect();
    format!("{cut}…")
}

/// Reconstruct a transcript from an audit log.
///
/// This is how a session driven by a client we do not control gets recorded.
/// The log holds a `pre` record (tool, redacted args, decision) and a `post`
/// record (ok, error code, latency) per call, which is exactly a turn.
pub fn from_audit(jsonl: &str, client: &str, task: &str) -> Result<Transcript, String> {
    let mut t = Transcript::new(client, None, task);
    let mut started = None;
    for (n, line) in jsonl.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let rec: Value = serde_json::from_str(line)
            .map_err(|e| format!("audit line {} is not JSON: {e}", n + 1))?;
        let Some(tool) = rec.get("tool").and_then(Value::as_str) else {
            continue;
        };
        // Protocol-level records (`resources/read`) are not agent tool calls.
        if tool.contains('/') {
            continue;
        }
        let ts = rec.get("ts_ms").and_then(Value::as_u64).unwrap_or(0) as u128;
        started.get_or_insert(ts);
        match rec.get("phase").and_then(Value::as_str) {
            Some("pre") => t.turns.push(Turn {
                role: "tool".to_string(),
                text: None,
                tool: Some(tool.to_string()),
                args: rec.get("args_redacted").cloned(),
                decision: rec
                    .get("decision")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                result: None,
            }),
            Some("post") => {
                // Fill the most recent unfinished call of this tool. Calls are
                // serialized by the server, so the newest match is the one.
                if let Some(turn) = t
                    .turns
                    .iter_mut()
                    .rev()
                    .find(|x| x.tool.as_deref() == Some(tool) && x.result.is_none())
                {
                    turn.result = Some(ResultSummary {
                        ok: rec.get("ok").and_then(Value::as_bool).unwrap_or(false),
                        error_code: rec
                            .get("error_code")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        latency_ms: rec.get("latency_ms").and_then(Value::as_u64).unwrap_or(0)
                            as u128,
                        excerpt: String::new(),
                    });
                }
            }
            _ => {}
        }
    }
    if let Some(ts) = started {
        t.started_ms = ts;
    }
    Ok(t)
}

/// Check a transcript is well formed and names only real tools.
///
/// Run against any recorded fixture in CI, so a stale demo artifact that refers
/// to a tool we renamed fails loudly instead of misleading a reader.
pub fn validate(v: &Value, known_tools: &[String]) -> Result<(), String> {
    if v.get("schema").and_then(Value::as_str) != Some(SCHEMA) {
        return Err(format!("schema is not {SCHEMA}"));
    }
    for field in ["client", "task"] {
        if !v.get(field).is_some_and(Value::is_string) {
            return Err(format!("'{field}' is missing or not a string"));
        }
    }
    let turns = v
        .get("turns")
        .and_then(Value::as_array)
        .ok_or("'turns' is missing or not an array")?;
    for (i, turn) in turns.iter().enumerate() {
        let role = turn
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("turn {i} has no role"))?;
        if !["user", "model", "tool"].contains(&role) {
            return Err(format!("turn {i} has an unknown role '{role}'"));
        }
        let Some(tool) = turn.get("tool").and_then(Value::as_str) else {
            continue;
        };
        if !known_tools.is_empty() && !known_tools.iter().any(|k| k == tool) {
            return Err(format!("turn {i} calls '{tool}', which is not a tool"));
        }
        if let Some(args) = turn.get("args") {
            if !args.is_object() && !args.is_null() {
                return Err(format!("turn {i} has non-object args"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_excerpt_is_bounded_and_never_splits_a_character() {
        let long = json!("é".repeat(500));
        let e = excerpt(&long);
        assert_eq!(e.chars().count(), EXCERPT_CHARS + 1); // + the ellipsis
        assert!(e.ends_with('…'));
        // Short values are untouched, and newlines are flattened so a
        // transcript stays one line per turn.
        assert_eq!(excerpt(&json!("a\nb")), "a b");
        assert_eq!(excerpt(&json!({ "ok": true })), r#"{"ok":true}"#);
    }

    #[test]
    fn an_audit_log_becomes_a_transcript() {
        let log = [
            r#"{"phase":"pre","ts_ms":1000,"session_id":"s","tool":"launch","tier":"standard","decision":"allow","args_redacted":{"app":"TextEdit"}}"#,
            r#"{"phase":"post","ts_ms":1200,"session_id":"s","tool":"launch","ok":true,"latency_ms":180}"#,
            r#"{"phase":"pre","ts_ms":1300,"session_id":"s","tool":"resources/read","decision":"resource"}"#,
            r#"{"phase":"pre","ts_ms":1400,"session_id":"s","tool":"fs_delete","decision":"deny"}"#,
        ]
        .join("\n");
        let t = from_audit(&log, "claude-code", "write a note").unwrap();
        assert_eq!(t.schema, SCHEMA);
        assert_eq!(t.started_ms, 1000);
        // The resource read is not an agent tool call.
        assert_eq!(t.turns.len(), 2);
        assert_eq!(t.turns[0].tool.as_deref(), Some("launch"));
        assert_eq!(t.turns[0].args.as_ref().unwrap()["app"], "TextEdit");
        assert_eq!(t.turns[0].result.as_ref().unwrap().latency_ms, 180);
        // A denied call has no post record, and must still appear: a demo that
        // silently dropped its refusals would be a misleading demo.
        assert_eq!(t.turns[1].decision.as_deref(), Some("deny"));
        assert!(t.turns[1].result.is_none());
        assert_eq!(t.tally(), (1, 2));
    }

    #[test]
    fn a_post_matches_the_newest_unfinished_call_of_that_tool() {
        let log = [
            r#"{"phase":"pre","ts_ms":1,"tool":"ping"}"#,
            r#"{"phase":"pre","ts_ms":2,"tool":"ping"}"#,
            r#"{"phase":"post","ts_ms":3,"tool":"ping","ok":true,"latency_ms":5}"#,
        ]
        .join("\n");
        let t = from_audit(&log, "c", "t").unwrap();
        assert!(t.turns[0].result.is_none());
        assert!(t.turns[1].result.is_some());
    }

    #[test]
    fn validation_rejects_a_transcript_naming_a_tool_that_does_not_exist() {
        let known = vec!["ping".to_string()];
        let mut t = Transcript::new("gemini-bridge", None, "say hello");
        t.tool_call(
            "ping",
            json!({}),
            ResultSummary {
                ok: true,
                error_code: None,
                latency_ms: 1,
                excerpt: "pong".into(),
            },
        );
        validate(&t.to_json(), &known).unwrap();

        let mut bad = t.clone();
        bad.turns[0].tool = Some("teleport".into());
        let e = validate(&bad.to_json(), &known).unwrap_err();
        assert!(e.contains("teleport"));

        let mut wrong_schema = t.to_json();
        wrong_schema["schema"] = json!("something/2");
        assert!(validate(&wrong_schema, &known).is_err());
    }

    #[test]
    fn a_malformed_audit_line_is_an_error_not_a_panic() {
        assert!(from_audit("not json", "c", "t").is_err());
        assert!(from_audit("", "c", "t").unwrap().turns.is_empty());
    }
}
