use async_trait::async_trait;
use mcp_types::{CallCtx, Category, Envelope, ErrorCode, Tier, ToolDescriptor, ToolModule};
use serde_json::{json, Value};

use crate::brew::{self, BrewError};
use crate::policy::{valid_package_id, valid_version, PkgPolicy, Refusal};

pub struct PkgModule {
    policy: PkgPolicy,
}

impl PkgModule {
    pub fn new(policy: PkgPolicy) -> Self {
        PkgModule { policy }
    }
}

fn refuse(tool: &str, r: Refusal) -> Envelope {
    let code = match r {
        Refusal::BadId(_) => ErrorCode::InvalidArgs,
        _ => ErrorCode::PolicyDenied,
    };
    Envelope::fail(tool, code, r.message())
}

fn brew_err(tool: &str, e: BrewError) -> Envelope {
    match e {
        BrewError::Missing(m) => Envelope::fail_with(
            tool,
            ErrorCode::UnsupportedOs,
            m,
            "install Homebrew, or configure a different source in packages.allowed_sources",
        ),
        BrewError::Timeout(m) => Envelope::fail(tool, ErrorCode::Timeout, m),
        BrewError::Failed(m) => Envelope::fail(tool, ErrorCode::ActionFailed, m),
    }
}

fn source_of(args: &Value) -> String {
    args.get("source")
        .and_then(Value::as_str)
        .unwrap_or("brew")
        .to_string()
}

fn id_of(args: &Value) -> Option<&str> {
    args.get("id").and_then(Value::as_str)
}

impl PkgModule {
    fn t(&self) -> u64 {
        self.policy.timeout_secs
    }

    async fn search(&self, args: &Value) -> Envelope {
        let tool = "app_search";
        let Some(query) = args
            .get("query")
            .and_then(Value::as_str)
            .filter(|q| !q.is_empty())
        else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'query'");
        };
        // The query reaches brew as argv; a leading '-' would still be an option.
        if query.starts_with('-') || query.len() > 128 {
            return Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                "'query' must not start with '-' and must be under 128 characters",
            );
        }
        let source = source_of(args);
        if let Err(r) = self.policy.check_source(&source) {
            return refuse(tool, r);
        }
        match brew::brew(&["search", query], self.t()).await {
            Ok(text) => {
                let mut formulae = Vec::new();
                let mut casks = Vec::new();
                let mut in_casks = false;
                for line in text.lines() {
                    let l = line.trim();
                    if l.starts_with("==>") {
                        in_casks = l.to_lowercase().contains("cask");
                        continue;
                    }
                    if l.is_empty() || !valid_package_id(l) {
                        continue;
                    }
                    if in_casks {
                        casks.push(json!({ "id": l, "kind": "cask", "source": source }));
                    } else {
                        formulae.push(json!({ "id": l, "kind": "formula", "source": source }));
                    }
                }
                let count = formulae.len() + casks.len();
                Envelope::ok(
                    tool,
                    json!({ "formulae": formulae, "casks": casks, "count": count }),
                )
            }
            Err(e) => brew_err(tool, e),
        }
    }

    async fn list_installed(&self, args: &Value) -> Envelope {
        let tool = "app_list_installed";
        let source = source_of(args);
        if let Err(r) = self.policy.check_source(&source) {
            return refuse(tool, r);
        }
        match brew::installed(self.t()).await {
            Ok(pkgs) => Envelope::ok(tool, json!({ "packages": pkgs, "count": pkgs.len() })),
            Err(e) => brew_err(tool, e),
        }
    }

    async fn info(&self, args: &Value) -> Envelope {
        let tool = "app_info";
        let Some(id) = id_of(args) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'id'");
        };
        let source = source_of(args);
        if let Err(r) = self.policy.check_lookup(id, &source) {
            return refuse(tool, r);
        }
        let cask = brew::is_cask(id, self.t()).await;
        match brew::info_json(id, cask, self.t()).await {
            Ok(v) => Envelope::ok(tool, brew::summarize(&v, id, cask)),
            Err(e) => brew_err(tool, e),
        }
    }

    /// The dry run. Read-tier and the intended first call: it surfaces the
    /// transitive dependencies *before* consent, so a human approves what will
    /// actually land rather than just the package that was named.
    async fn install_plan(&self, args: &Value) -> Envelope {
        let tool = "app_install_plan";
        let Some(id) = id_of(args) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'id'");
        };
        let source = source_of(args);
        if let Err(r) = self.policy.check_install(id, &source) {
            return refuse(tool, r);
        }
        let cask = brew::is_cask(id, self.t()).await;
        let info = match brew::info_json(id, cask, self.t()).await {
            Ok(v) => brew::summarize(&v, id, cask),
            Err(e) => return brew_err(tool, e),
        };
        if info["found"] != json!(true) {
            return Envelope::fail(
                tool,
                ErrorCode::NotFound,
                format!("'{id}' is not in the {source} index"),
            );
        }
        let deps = brew::deps(id, self.t()).await.unwrap_or_default();
        let already = info["installed"] == json!(true);
        Envelope::ok(
            tool,
            json!({
                "id": info["id"], "source": source, "kind": info["kind"],
                "resolved_version": info["version"],
                "already_installed": already,
                "installed_version": info["installed_version"],
                "transitive_dependencies": deps,
                "dependency_count": deps.len(),
                // Casks drop an app bundle into /Applications and frequently run
                // an installer that asks for an administrator password.
                "requires_elevation": cask,
                "deprecated": info["deprecated"],
                "homepage": info["homepage"],
                "dry_run": true,
            }),
        )
    }

    async fn install(&self, args: &Value) -> Envelope {
        let tool = "app_install";
        let Some(id) = id_of(args) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'id'");
        };
        let source = source_of(args);
        if let Err(r) = self.policy.check_install(id, &source) {
            return refuse(tool, r);
        }
        let version = args.get("version").and_then(Value::as_str);
        if let Some(v) = version {
            if !valid_version(v) {
                return Envelope::fail(
                    tool,
                    ErrorCode::InvalidArgs,
                    format!("'{v}' is not a valid version string"),
                );
            }
        }
        let cask = brew::is_cask(id, self.t()).await;
        // Resolve *before* installing so the audit records the identity that was
        // actually fetched, not the string that was asked for.
        let resolved = match brew::info_json(id, cask, self.t()).await {
            Ok(v) => brew::summarize(&v, id, cask),
            Err(e) => return brew_err(tool, e),
        };
        if resolved["found"] != json!(true) {
            return Envelope::fail(tool, ErrorCode::NotFound, format!("'{id}' not found"));
        }
        // Note what is absent: no --force, no --no-verify, no
        // HOMEBREW_NO_INSTALL_FROM_API. If the manager refuses a package, that
        // refusal is the answer.
        let mut argv: Vec<&str> = vec!["install"];
        if cask {
            argv.push("--cask");
        }
        argv.push(id);
        match brew::brew(&argv, self.t()).await {
            Ok(log) => Envelope::ok(
                tool,
                json!({
                    "installed": true, "id": resolved["id"], "source": source,
                    "resolved_version": resolved["version"], "kind": resolved["kind"],
                    "log_tail": tail(&log, 40),
                }),
            ),
            Err(e) => brew_err(tool, e),
        }
    }

    async fn uninstall(&self, args: &Value) -> Envelope {
        let tool = "app_uninstall";
        let Some(id) = id_of(args) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'id'");
        };
        let source = source_of(args);
        if let Err(r) = self.policy.check_uninstall(id, &source) {
            return refuse(tool, r);
        }
        let purge = args.get("purge").and_then(Value::as_bool).unwrap_or(false);
        let cask = brew::is_cask(id, self.t()).await;
        let mut argv: Vec<&str> = vec!["uninstall"];
        if cask {
            argv.push("--cask");
            if purge {
                // Cask-only: also removes the app's preferences and data.
                argv.push("--zap");
            }
        }
        argv.push(id);
        match brew::brew(&argv, self.t()).await {
            Ok(log) => Envelope::ok(
                tool,
                json!({ "uninstalled": true, "id": id, "source": source,
                        "purged": purge && cask, "log_tail": tail(&log, 40) }),
            ),
            Err(e) => brew_err(tool, e),
        }
    }

    async fn update(&self, args: &Value) -> Envelope {
        let tool = "app_update";
        let source = source_of(args);
        if let Err(r) = self.policy.check_source(&source) {
            return refuse(tool, r);
        }
        let mut argv: Vec<&str> = vec!["upgrade"];
        if let Some(id) = id_of(args) {
            if let Err(r) = self.policy.check_install(id, &source) {
                return refuse(tool, r);
            }
            argv.push(id);
        }
        match brew::brew(&argv, self.t()).await {
            Ok(log) => Envelope::ok(
                tool,
                json!({ "updated": true, "scope": id_of(args).unwrap_or("all"),
                        "log_tail": tail(&log, 60) }),
            ),
            Err(e) => brew_err(tool, e),
        }
    }
}

/// Keep the end of a manager's log: the outcome is at the bottom.
fn tail(text: &str, lines: usize) -> Vec<String> {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

#[async_trait]
impl ToolModule for PkgModule {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        let src = json!({ "type": "string", "description": "package manager, e.g. brew" });
        vec![
            ToolDescriptor::new(
                "app_search",
                Category::Packages,
                Tier::Read,
                "Search the package index.",
                json!({"type":"object","properties":{
                    "query":{"type":"string"},"source":src},"required":["query"]}),
            )
            .untrusted_output(),
            ToolDescriptor::new(
                "app_list_installed",
                Category::Packages,
                Tier::Read,
                "List installed packages with their versions.",
                json!({"type":"object","properties":{"source":src},"required":[]}),
            ),
            ToolDescriptor::new(
                "app_info",
                Category::Packages,
                Tier::Read,
                "Version, description, licence, homepage and declared dependencies.",
                json!({"type":"object","properties":{
                    "id":{"type":"string"},"source":src},"required":["id"]}),
            )
            .untrusted_output(),
            ToolDescriptor::new(
                "app_install_plan",
                Category::Packages,
                Tier::Read,
                "Dry run: what an install would actually bring in, that is the resolved version, the full \
                 transitive dependency list, and whether elevation is needed. Call this first, so \
                 approval covers what will land rather than just the package you named.",
                json!({"type":"object","properties":{
                    "id":{"type":"string"},"source":src,"version":{"type":"string"}},
                    "required":["id"]}),
            ),
            ToolDescriptor::new(
                "app_install",
                Category::Packages,
                Tier::Dangerous,
                "Install a package. Arbitrary sources (URLs, local files, third-party taps) are \
                 off unless packages.allow_arbitrary_source is set, and no flag that weakens \
                 signature or checksum verification is reachable from here.",
                json!({"type":"object","properties":{
                    "id":{"type":"string"},"source":src,"version":{"type":"string"},
                    "accept_deps":{"type":"boolean"}},"required":["id"]}),
            ),
            ToolDescriptor::new(
                "app_uninstall",
                Category::Packages,
                Tier::Dangerous,
                "Remove a package. 'purge' also removes its configuration and data. A protected \
                 set (this agent, the package manager, security tooling) can never be removed.",
                json!({"type":"object","properties":{
                    "id":{"type":"string"},"source":src,"purge":{"type":"boolean"}},
                    "required":["id"]}),
            ),
            ToolDescriptor::new(
                "app_update",
                Category::Packages,
                Tier::Dangerous,
                "Update one package, or all of them when 'id' is omitted.",
                json!({"type":"object","properties":{
                    "id":{"type":"string"},"source":src},"required":[]}),
            ),
        ]
    }

    /// Consent names the package and what the change is. `app_install_plan` is
    /// what should have run first; the prompt says so when it plainly did not.
    fn consent_prompt(&self, name: &str, args: &Value) -> Option<String> {
        let id = id_of(args).unwrap_or("all packages");
        let source = source_of(args);
        match name {
            "app_install" => Some(format!(
                "Install '{id}' from {source}? Installing runs the publisher's code on this \
                 machine and leaves it there. Run app_install_plan first to see the full \
                 dependency list."
            )),
            "app_uninstall" => Some(format!(
                "Uninstall '{id}'{}?",
                if args.get("purge").and_then(Value::as_bool) == Some(true) {
                    " and permanently delete its configuration and data"
                } else {
                    ""
                }
            )),
            "app_update" => Some(match id_of(args) {
                Some(i) => format!("Update '{i}' from {source}?"),
                None => format!(
                    "Update *every* package from {source}? This can change or break \
                                 software other things depend on."
                ),
            }),
            _ => None,
        }
    }

    async fn call(&self, name: &str, args: Value, _ctx: &CallCtx) -> Envelope {
        match name {
            "app_search" => self.search(&args).await,
            "app_list_installed" => self.list_installed(&args).await,
            "app_info" => self.info(&args).await,
            "app_install_plan" => self.install_plan(&args).await,
            "app_install" => self.install(&args).await,
            "app_uninstall" => self.uninstall(&args).await,
            "app_update" => self.update(&args).await,
            other => Envelope::fail(other, ErrorCode::InvalidArgs, "unknown tool"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_types::CancelToken;

    fn ctx() -> CallCtx {
        CallCtx::new("t", CancelToken::new())
    }
    fn open() -> PkgModule {
        PkgModule::new(PkgPolicy {
            allowed_sources: vec!["brew".into()],
            ..PkgPolicy::default()
        })
    }

    #[tokio::test]
    async fn closed_by_default_refuses_every_source() {
        let m = PkgModule::new(PkgPolicy::default());
        for (tool, args) in [
            ("app_search", json!({ "query": "wget" })),
            ("app_list_installed", json!({})),
            ("app_info", json!({ "id": "wget" })),
            ("app_install_plan", json!({ "id": "wget" })),
            ("app_install", json!({ "id": "wget" })),
            ("app_uninstall", json!({ "id": "wget" })),
            ("app_update", json!({})),
        ] {
            let env = m.call(tool, args, &ctx()).await;
            assert!(!env.ok, "{tool} must be closed by default");
            assert_eq!(env.error.unwrap().code, ErrorCode::PolicyDenied, "{tool}");
        }
    }

    /// The sharp edge: a package "name" that is really an option.
    #[tokio::test]
    async fn option_shaped_ids_never_reach_the_manager() {
        let m = open();
        for bad in ["--force", "-f", "--no-verify"] {
            for tool in [
                "app_info",
                "app_install",
                "app_uninstall",
                "app_install_plan",
            ] {
                let env = m.call(tool, json!({ "id": bad }), &ctx()).await;
                assert!(!env.ok, "{tool} accepted {bad}");
                assert_eq!(
                    env.error.unwrap().code,
                    ErrorCode::InvalidArgs,
                    "{tool} {bad}"
                );
            }
        }
        let env = m
            .call("app_search", json!({ "query": "--version" }), &ctx())
            .await;
        assert!(!env.ok, "search query must be checked too");
    }

    #[tokio::test]
    async fn protected_packages_can_never_be_uninstalled() {
        let m = open();
        for p in ["agentctl", "brew", "openssl@3", "santa"] {
            let env = m.call("app_uninstall", json!({ "id": p }), &ctx()).await;
            assert!(!env.ok, "{p}");
            let e = env.error.unwrap();
            assert_eq!(e.code, ErrorCode::PolicyDenied);
            assert!(e.message.contains("protected"), "{}", e.message);
        }
    }

    #[tokio::test]
    async fn every_mutation_asks_and_every_read_does_not() {
        let m = open();
        for t in ["app_install", "app_uninstall", "app_update"] {
            assert!(
                m.consent_prompt(t, &json!({ "id": "wget" })).is_some(),
                "{t} must ask"
            );
        }
        for t in [
            "app_search",
            "app_list_installed",
            "app_info",
            "app_install_plan",
        ] {
            assert!(
                m.consent_prompt(t, &json!({ "id": "wget" })).is_none(),
                "{t}"
            );
        }
        // Updating everything is a bigger claim than updating one thing.
        let all = m.consent_prompt("app_update", &json!({})).unwrap();
        assert!(all.contains("*every*"), "{all}");
    }

    #[tokio::test]
    async fn purge_is_named_in_the_prompt() {
        let m = open();
        let p = m
            .consent_prompt("app_uninstall", &json!({ "id": "wget", "purge": true }))
            .unwrap();
        assert!(p.contains("configuration and data"), "{p}");
    }
}
