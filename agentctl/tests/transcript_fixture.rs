//! Any recorded demo transcript in the repo must still describe reality.
//!
//! A demo artifact is the one document nobody re-reads, so it is the one most
//! likely to keep claiming a tool we renamed six months ago. The fixtures are
//! optional; when they exist they are checked.

use mcp_policy::PolicyConfig;
use serde_json::Value;

fn known_tools() -> Vec<String> {
    agentctl::build_modules(&PolicyConfig::default())
        .iter()
        .flat_map(|m| m.descriptors())
        .map(|d| d.name.clone())
        .collect()
}

#[test]
fn recorded_transcripts_are_valid_and_name_real_tools() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("docs/fixtures");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return; // No recordings yet.
    };
    let tools = known_tools();
    let mut checked = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let v: Value = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("{} is not JSON: {e}", path.display()));
        if v.get("schema").and_then(Value::as_str) != Some(agentctl::transcript::SCHEMA) {
            continue; // Some other fixture.
        }
        agentctl::transcript::validate(&v, &tools)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        checked += 1;
    }
    eprintln!("checked {checked} transcript fixture(s)");
}
