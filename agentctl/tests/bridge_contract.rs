//! The tool surface has to stay inside what a Gemini function declaration can
//! express.
//!
//! This is the linter the reference client makes possible. Gemini does not
//! reject one bad declaration — it rejects the request, so a single tool that
//! grows an `additionalProperties` takes all 107 down with it, at the far end
//! of a network call, in front of whoever is running the demo. Cheaper to fail
//! here.

use agentctl::bridge::gemini;
use mcp_policy::PolicyConfig;
use serde_json::{json, Value};

/// Every tool, regardless of which categories an operator enabled.
fn every_tool() -> Vec<Value> {
    agentctl::build_modules(&PolicyConfig::default())
        .iter()
        .flat_map(|m| m.descriptors())
        .map(|d| {
            json!({
                "name": d.name,
                "description": d.description,
                "inputSchema": d.input_schema,
            })
        })
        .collect()
}

#[test]
fn every_input_schema_is_already_gemini_safe() {
    let tools = every_tool();
    assert!(
        tools.len() > 70,
        "expected the full catalog, got {}",
        tools.len()
    );
    let mut rewritten = Vec::new();
    for tool in &tools {
        let schema = &tool["inputSchema"];
        if gemini::sanitize_schema(schema) != *schema {
            rewritten.push(tool["name"].as_str().unwrap_or("?").to_string());
        }
    }
    assert!(
        rewritten.is_empty(),
        "these tools use schema keywords Gemini rejects (pattern/format/\
         additionalProperties/oneOf/…): {rewritten:?}"
    );
}

#[test]
fn every_tool_produces_a_declaration_the_api_will_accept() {
    for tool in every_tool() {
        let name = tool["name"].as_str().unwrap();
        let decl =
            gemini::declaration(&tool).unwrap_or_else(|| panic!("{name} produced no declaration"));

        // Gemini's own constraint on function names.
        assert!(
            name.len() <= 64
                && name
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.'),
            "'{name}' is not a valid function name"
        );
        let description = decl["description"].as_str().unwrap_or_default();
        assert!(
            !description.is_empty(),
            "{name} has no description; a model cannot choose a tool it cannot read about"
        );

        // A declared object schema must actually declare its properties, or
        // the model has to guess the argument names.
        if let Some(params) = decl.get("parameters") {
            assert_eq!(
                params["type"], "object",
                "{name} takes a non-object argument"
            );
            let props = params["properties"].as_object().unwrap();
            for required in params
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let key = required.as_str().unwrap_or_default();
                assert!(
                    props.contains_key(key),
                    "{name} requires '{key}' but does not describe it"
                );
            }
        }
    }
}

/// The bridge only ever offers what the server actually advertises, so a tool
/// that is gated off cannot be called by the model. Cheap to state, and it is
/// the property that keeps the policy gate meaningful for this client.
#[test]
fn declarations_are_built_from_the_servers_own_list() {
    let visible = vec![json!({
        "name": "ping",
        "description": "Health check.",
        "inputSchema": { "type": "object", "properties": {}, "required": [] }
    })];
    let block = gemini::tools_block(&visible);
    let decls = block[0]["functionDeclarations"].as_array().unwrap();
    assert_eq!(decls.len(), 1);
    assert_eq!(decls[0]["name"], "ping");
}
