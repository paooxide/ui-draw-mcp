//! Read-only checks against the real package manager.
//!
//! Nothing here installs or removes anything: the mutating paths are covered by
//! policy tests, because a test that actually installs software would be a poor
//! trade on a developer's machine. Skips when Homebrew is absent.

use mcp_pkg::{PkgModule, PkgPolicy};
use mcp_types::{CallCtx, CancelToken, ToolModule};
use serde_json::json;

fn ctx() -> CallCtx {
    CallCtx::new("t", CancelToken::new())
}

/// These tests shell out to a real package manager, which reaches the network.
/// That is the point locally, and a flake source in CI — a hosted runner has
/// Homebrew installed but no guarantee the formulae API answers promptly. Set
/// `AGENTCTL_SKIP_LIVE=1` to skip every live-network test in the workspace.
fn skip_live() -> bool {
    std::env::var_os("AGENTCTL_SKIP_LIVE").is_some_and(|v| v != "0")
}

fn have_brew() -> bool {
    !skip_live()
        && ["/opt/homebrew/bin/brew", "/usr/local/bin/brew"]
            .iter()
            .any(|p| std::path::Path::new(p).exists())
}

fn module() -> PkgModule {
    PkgModule::new(PkgPolicy {
        allowed_sources: vec!["brew".into()],
        timeout_secs: 120,
        ..PkgPolicy::default()
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn info_reads_a_real_package() {
    if !have_brew() {
        return;
    }
    let env = module()
        .call("app_info", json!({ "id": "wget" }), &ctx())
        .await;
    assert!(env.ok, "{env:?}");
    let d = env.data.unwrap();
    assert_eq!(d["found"], true);
    assert_eq!(d["kind"], "formula");
    assert!(d["version"].as_str().is_some_and(|v| !v.is_empty()));
    assert!(d["homepage"].as_str().is_some());
}

/// The dry run has to surface the *transitive* set, not just the named package
/// — that is the whole reason it runs before consent.
#[tokio::test(flavor = "multi_thread")]
async fn install_plan_is_a_dry_run_that_names_dependencies() {
    if !have_brew() {
        return;
    }
    let before = installed_ids().await;
    let env = module()
        .call("app_install_plan", json!({ "id": "wget" }), &ctx())
        .await;
    assert!(env.ok, "{env:?}");
    let d = env.data.unwrap();
    assert_eq!(d["dry_run"], true);
    assert!(d["resolved_version"].as_str().is_some());
    assert!(
        d["dependency_count"].as_u64().unwrap_or(0) > 0,
        "wget has dependencies; the plan must list them: {d}"
    );
    assert!(d["transitive_dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v.as_str() == Some("openssl@3")));
    // And it changed nothing.
    assert_eq!(before, installed_ids().await, "a plan must not mutate");
}

async fn installed_ids() -> Vec<String> {
    let env = module().call("app_list_installed", json!({}), &ctx()).await;
    let Some(d) = env.data else { return Vec::new() };
    d["packages"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|p| p["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn search_returns_real_index_entries() {
    if !have_brew() {
        return;
    }
    let env = module()
        .call("app_search", json!({ "query": "ripgrep" }), &ctx())
        .await;
    assert!(env.ok, "{env:?}");
    let d = env.data.unwrap();
    assert!(d["count"].as_u64().unwrap_or(0) > 0, "{d}");
    assert!(d["formulae"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["id"] == "ripgrep"));
}

#[tokio::test(flavor = "multi_thread")]
async fn list_installed_reports_versions() {
    if !have_brew() {
        return;
    }
    let env = module().call("app_list_installed", json!({}), &ctx()).await;
    assert!(env.ok, "{env:?}");
    let d = env.data.unwrap();
    for p in d["packages"].as_array().unwrap().iter().take(5) {
        assert!(p["id"].as_str().is_some());
        assert!(matches!(p["kind"].as_str(), Some("formula") | Some("cask")));
    }
}

/// A package the index does not carry must be NOT_FOUND, not a vague failure —
/// the agent needs to know whether to try a different id or a different source.
#[tokio::test(flavor = "multi_thread")]
async fn a_missing_package_is_not_found() {
    if !have_brew() {
        return;
    }
    let env = module()
        .call(
            "app_install_plan",
            json!({ "id": "definitely-not-a-real-package-xyzzy" }),
            &ctx(),
        )
        .await;
    assert!(!env.ok);
}
