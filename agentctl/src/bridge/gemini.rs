//! Translation between MCP tool descriptors and the Gemini `generateContent`
//! API.
//!
//! Everything here is a value transformation, so the whole mapping is testable
//! without a key, a network or a subprocess. The parts that touch the outside
//! world live in [`super::curl`] and [`super::mcp_child`].

use serde_json::{json, Map, Value};

/// The schema keys a Gemini function declaration understands.
///
/// `parameters` is an OpenAPI 3.0 subset, and an unknown key is not ignored:
/// the request fails, taking every *other* tool declaration down with it. Every
/// descriptor in this workspace is already inside this subset — a test asserts
/// that they are unchanged by the sanitizer — so this exists as a guard, not a
/// translation. A tool that later grows a `pattern` degrades to a working
/// declaration with a slightly looser contract instead of breaking the run.
const ALLOWED_SCHEMA_KEYS: &[&str] = &[
    "type",
    "description",
    "enum",
    "items",
    "properties",
    "required",
    "nullable",
];

/// Recursively drop schema keys Gemini does not accept.
pub fn sanitize_schema(v: &Value) -> Value {
    let Some(obj) = v.as_object() else {
        return v.clone();
    };
    let mut out = Map::new();
    for (key, val) in obj {
        if !ALLOWED_SCHEMA_KEYS.contains(&key.as_str()) {
            continue;
        }
        let cleaned = match key.as_str() {
            "properties" => {
                let mut props = Map::new();
                for (name, sub) in val.as_object().into_iter().flatten() {
                    props.insert(name.clone(), sanitize_schema(sub));
                }
                Value::Object(props)
            }
            "items" => sanitize_schema(val),
            _ => val.clone(),
        };
        out.insert(key.clone(), cleaned);
    }
    Value::Object(out)
}

/// One entry of `tools/list` as a Gemini function declaration.
///
/// `parameters` is omitted for a tool that takes none: an empty object is
/// rejected as a malformed schema.
pub fn declaration(tool: &Value) -> Option<Value> {
    let name = tool.get("name")?.as_str()?;
    let description = tool
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut decl = json!({ "name": name, "description": description });
    let schema = tool.get("inputSchema").map(sanitize_schema);
    let has_properties = schema
        .as_ref()
        .and_then(|s| s.get("properties"))
        .and_then(Value::as_object)
        .is_some_and(|p| !p.is_empty());
    if has_properties {
        decl["parameters"] = schema.unwrap_or(Value::Null);
    }
    Some(decl)
}

/// The whole `tools` block for a request.
pub fn tools_block(tools: &[Value]) -> Value {
    let decls: Vec<Value> = tools.iter().filter_map(declaration).collect();
    json!([{ "functionDeclarations": decls }])
}

/// Everything about a request that is not the conversation itself.
pub struct RequestOpts<'a> {
    pub system: Option<&'a str>,
    /// `AUTO` lets the model answer in prose; `ANY` forces a tool call.
    pub mode: &'a str,
    /// Gemini 3 accepts `thinkingLevel`; older models do not. Left unset by
    /// default so the bridge works against whatever model is named.
    pub thinking_level: Option<&'a str>,
}

/// Build a `generateContent` body.
///
/// Deliberately sets no `temperature`, `topP` or `topK`: the tuned defaults
/// differ per model, and overriding them is a known way to make function
/// calling worse.
pub fn request_body(contents: &[Value], tools: &Value, opts: &RequestOpts) -> Value {
    let mut body = json!({
        "contents": contents,
        "tools": tools,
        "toolConfig": { "functionCallingConfig": { "mode": opts.mode } },
    });
    if let Some(system) = opts.system {
        body["system_instruction"] = json!({ "parts": [{ "text": system }] });
    }
    if let Some(level) = opts.thinking_level {
        body["generationConfig"] = json!({ "thinkingConfig": { "thinkingLevel": level } });
    }
    body
}

/// A tool call the model asked for.
#[derive(Debug, Clone, PartialEq)]
pub struct FunctionCall {
    pub name: String,
    pub args: Value,
}

/// One model turn, parsed.
#[derive(Debug, Clone)]
pub struct ModelTurn {
    /// The candidate's `content`, kept verbatim so it can be echoed back into
    /// the next request. Gemini 3 returns a `thoughtSignature` on the part that
    /// made a call and rejects a follow-up that dropped it, so this must never
    /// be rebuilt from the parsed fields.
    pub content: Value,
    pub text: String,
    pub calls: Vec<FunctionCall>,
    pub finish_reason: Option<String>,
}

impl ModelTurn {
    /// The model asked for nothing further.
    pub fn is_final(&self) -> bool {
        self.calls.is_empty()
    }
}

/// Parse a `generateContent` response.
pub fn parse_response(v: &Value) -> Result<ModelTurn, String> {
    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        let status = err.get("status").and_then(Value::as_str).unwrap_or("");
        return Err(if status.is_empty() {
            format!("Gemini API error: {msg}")
        } else {
            format!("Gemini API error ({status}): {msg}")
        });
    }
    if let Some(reason) = v
        .pointer("/promptFeedback/blockReason")
        .and_then(Value::as_str)
    {
        return Err(format!("the prompt was blocked: {reason}"));
    }
    let Some(candidate) = v.pointer("/candidates/0") else {
        return Err("the response carried no candidates".into());
    };
    let finish_reason = candidate
        .get("finishReason")
        .and_then(Value::as_str)
        .map(str::to_string);
    let content = candidate
        .get("content")
        .cloned()
        .unwrap_or_else(|| json!({ "role": "model", "parts": [] }));

    let mut text = String::new();
    let mut calls = Vec::new();
    for part in content
        .get("parts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        // A thought summary is for the operator to read, not part of the answer.
        let is_thought = part.get("thought").and_then(Value::as_bool) == Some(true);
        if let Some(t) = part.get("text").and_then(Value::as_str) {
            if !is_thought {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(t);
            }
        }
        if let Some(fc) = part.get("functionCall") {
            let Some(name) = fc.get("name").and_then(Value::as_str) else {
                continue;
            };
            calls.push(FunctionCall {
                name: name.to_string(),
                args: fc.get("args").cloned().unwrap_or_else(|| json!({})),
            });
        }
    }
    Ok(ModelTurn {
        content,
        text,
        calls,
        finish_reason,
    })
}

/// One `functionResponse` part. Gemini requires the payload to be an object,
/// so a non-object result is wrapped rather than sent bare.
pub fn function_response(name: &str, response: Value) -> Value {
    let response = if response.is_object() {
        response
    } else {
        json!({ "result": response })
    };
    json!({ "functionResponse": { "name": name, "response": response } })
}

/// A retry is worth one attempt when the model produced a call the API itself
/// could not parse; anything else is a real answer.
pub fn is_malformed_call(turn: &ModelTurn) -> bool {
    turn.finish_reason.as_deref() == Some("MALFORMED_FUNCTION_CALL")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sanitizer_drops_only_unknown_keys_and_recurses() {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": { "type": "string", "pattern": "^/", "description": "a path" },
                "tags": { "type": "array", "items": { "type": "string", "format": "uuid" } }
            },
            "required": ["path"]
        });
        let out = sanitize_schema(&schema);
        assert!(out.get("additionalProperties").is_none());
        assert!(out.pointer("/properties/path/pattern").is_none());
        assert_eq!(
            out.pointer("/properties/path/description").unwrap(),
            "a path"
        );
        assert!(out.pointer("/properties/tags/items/format").is_none());
        assert_eq!(
            out.pointer("/properties/tags/items/type").unwrap(),
            "string"
        );
        assert_eq!(out.get("required").unwrap(), &json!(["path"]));
    }

    #[test]
    fn a_tool_with_no_arguments_declares_no_parameters() {
        // An empty `parameters` object is rejected as a malformed schema, and
        // `ping` really does take nothing.
        let tool = json!({
            "name": "ping",
            "description": "Health check.",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        });
        let decl = declaration(&tool).unwrap();
        assert_eq!(decl["name"], "ping");
        assert!(decl.get("parameters").is_none());
    }

    #[test]
    fn a_tool_with_arguments_carries_a_sanitized_schema() {
        let tool = json!({
            "name": "launch",
            "description": "Launch an app.",
            "inputSchema": {
                "type": "object",
                "properties": { "app": { "type": "string" } },
                "required": ["app"],
                "additionalProperties": false
            }
        });
        let decl = declaration(&tool).unwrap();
        assert_eq!(decl["parameters"]["properties"]["app"]["type"], "string");
        assert!(decl["parameters"].get("additionalProperties").is_none());
    }

    #[test]
    fn a_nameless_entry_is_skipped_rather_than_breaking_the_list() {
        assert!(declaration(&json!({ "description": "no name" })).is_none());
        let block = tools_block(&[json!({ "description": "x" }), json!({ "name": "ping" })]);
        assert_eq!(
            block[0]["functionDeclarations"].as_array().unwrap().len(),
            1
        );
    }

    #[test]
    fn the_request_body_omits_the_sampling_knobs() {
        let body = request_body(
            &[json!({ "role": "user", "parts": [{ "text": "hi" }] })],
            &tools_block(&[]),
            &RequestOpts {
                system: Some("be careful"),
                mode: "AUTO",
                thinking_level: None,
            },
        );
        assert_eq!(body["toolConfig"]["functionCallingConfig"]["mode"], "AUTO");
        assert_eq!(body["system_instruction"]["parts"][0]["text"], "be careful");
        // Overriding these makes function calling measurably worse, and the
        // right values differ per model.
        assert!(body.get("generationConfig").is_none());
        for knob in ["temperature", "topP", "topK"] {
            assert!(body.pointer(&format!("/generationConfig/{knob}")).is_none());
        }
    }

    #[test]
    fn a_thinking_level_is_only_sent_when_asked_for() {
        let body = request_body(
            &[],
            &json!([]),
            &RequestOpts {
                system: None,
                mode: "ANY",
                thinking_level: Some("low"),
            },
        );
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "low"
        );
    }

    #[test]
    fn a_response_yields_its_calls_and_its_prose() {
        let v = json!({ "candidates": [{
            "finishReason": "STOP",
            "content": { "role": "model", "parts": [
                { "text": "I will look first." },
                { "functionCall": { "name": "get_ui_tree", "args": { "app": "TextEdit" } } }
            ]}
        }]});
        let turn = parse_response(&v).unwrap();
        assert_eq!(turn.text, "I will look first.");
        assert_eq!(turn.calls.len(), 1);
        assert_eq!(turn.calls[0].name, "get_ui_tree");
        assert_eq!(turn.calls[0].args["app"], "TextEdit");
        assert!(!turn.is_final());
    }

    /// Gemini 3 rejects a follow-up request whose echoed part lost its
    /// `thoughtSignature`, so the content must survive the round trip byte for
    /// byte rather than being rebuilt from the parsed fields.
    #[test]
    fn the_candidate_content_is_kept_verbatim() {
        let content = json!({ "role": "model", "parts": [
            { "functionCall": { "name": "ping", "args": {} }, "thoughtSignature": "sig-abc" }
        ]});
        let turn =
            parse_response(&json!({ "candidates": [{ "content": content.clone() }] })).unwrap();
        assert_eq!(turn.content, content);
    }

    #[test]
    fn a_thought_part_is_not_shown_as_the_answer() {
        let v = json!({ "candidates": [{ "content": { "parts": [
            { "text": "hmm, maybe the menu", "thought": true },
            { "text": "Done." }
        ]}}]});
        assert_eq!(parse_response(&v).unwrap().text, "Done.");
    }

    #[test]
    fn an_api_error_becomes_an_error_not_an_empty_turn() {
        let e = parse_response(&json!({
            "error": { "code": 400, "status": "INVALID_ARGUMENT", "message": "bad schema" }
        }))
        .unwrap_err();
        assert!(e.contains("INVALID_ARGUMENT") && e.contains("bad schema"));
        let blocked =
            parse_response(&json!({ "promptFeedback": { "blockReason": "SAFETY" } })).unwrap_err();
        assert!(blocked.contains("SAFETY"));
        assert!(parse_response(&json!({}))
            .unwrap_err()
            .contains("no candidates"));
    }

    #[test]
    fn a_non_object_result_is_wrapped_for_the_api() {
        let part = function_response("ping", json!("pong"));
        assert_eq!(part["functionResponse"]["response"]["result"], "pong");
        let obj = function_response("ping", json!({ "ok": true }));
        assert_eq!(obj["functionResponse"]["response"]["ok"], true);
    }
}
