//! The browser tool surface: what the server advertises is what it serves.

use std::sync::Arc;

use mcp_browser::{BrowserModule, CdpBackend, NavPolicy};
use mcp_types::{CallCtx, CancelToken, ToolModule};
use serde_json::json;

/// `browser_branch` and `browser_checkpoint` take an `action`. The per-action
/// spellings (`browser_branch_create`, `browser_checkpoint_save`, ...) once
/// existed only as dispatch aliases with no descriptor, so a client could never
/// discover them and the server refused them as unknown tools. They are gone;
/// this keeps them from creeping back as undiscoverable dispatch arms.
#[tokio::test]
async fn per_action_branch_and_checkpoint_aliases_are_not_dispatchable() {
    let m = BrowserModule::new(Arc::new(CdpBackend::new(NavPolicy::new(&[], true))));
    let advertised: Vec<String> = m.descriptors().iter().map(|d| d.name.to_string()).collect();
    assert!(advertised.iter().any(|n| n == "browser_branch"));
    assert!(advertised.iter().any(|n| n == "browser_checkpoint"));

    for alias in [
        "browser_branch_create",
        "browser_branch_commit",
        "browser_branch_discard",
        "browser_checkpoint_save",
        "browser_checkpoint_rollback",
    ] {
        assert!(!advertised.iter().any(|n| n == alias), "{alias} advertised");
        let e = m
            .call(alias, json!({}), &CallCtx::new("t", CancelToken::new()))
            .await;
        assert!(!e.ok, "{alias} must not dispatch");
        assert_eq!(
            e.error.as_ref().map(|x| x.message.as_str()),
            Some("unknown tool")
        );
    }
}
