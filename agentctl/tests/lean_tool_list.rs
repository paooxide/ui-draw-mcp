//! The browser tool list is re-sent to the model every turn, so its size is a
//! cost the model pays on every call. `browser-core` is the lean subset and the
//! descriptors keep long-form docs out of the wire text.

use std::sync::Arc;

use mcp_browser::{BrowserModule, CdpBackend, NavPolicy};
use mcp_policy::RoleProfile;
use mcp_types::{ToolDescriptor, ToolModule};

const CORE: [&str; 12] = [
    "browser_connect",
    "browser_tabs",
    "browser_navigate",
    "browser_snapshot",
    "browser_query",
    "browser_act",
    "browser_fill_form",
    "browser_wait",
    "browser_screenshot",
    "browser_extract",
    "browser_dialog",
    "browser_upload",
];

fn descriptors() -> Vec<ToolDescriptor> {
    BrowserModule::new(Arc::new(CdpBackend::new(NavPolicy::new(&[], true)))).descriptors()
}

/// Characters of what a model reads for one tool: name, description, schema.
fn wire_chars(d: &ToolDescriptor) -> usize {
    d.name.len() + d.description.len() + d.input_schema.to_string().len()
}

#[test]
fn browser_core_advertises_exactly_the_lean_set() {
    let role = RoleProfile::browser_core();
    let mut names: Vec<String> = descriptors()
        .iter()
        .filter(|d| role.allows_tool(d))
        .map(|d| d.name.clone())
        .collect();
    names.sort();
    let mut want: Vec<String> = CORE.iter().map(|s| s.to_string()).collect();
    want.sort();
    assert_eq!(names, want);
}

#[test]
fn browser_core_stays_small() {
    let role = RoleProfile::browser_core();
    let total: usize = descriptors()
        .iter()
        .filter(|d| role.allows_tool(d))
        .map(wire_chars)
        .sum();
    // About 2.3k tokens, at 4 chars to a token. A descriptor that grows past
    // this should move its prose into `details`. The ceiling was 11_000 before
    // `browser_screenshot` gained OCR (`ocr`, `find`, `exact`, `image`): the
    // parameters are the growth, their prose is in `details`.
    assert!(total < 11_300, "browser-core wire text is {total} chars");
}

#[test]
fn moved_prose_is_documented_not_dropped() {
    let all = descriptors();
    let act = all.iter().find(|d| d.name == "browser_act").unwrap();
    assert!(act
        .details
        .as_deref()
        .is_some_and(|t| t.contains("React-style")));
    assert!(!act.description.contains("React-style"));
}
