//! Homebrew backend. Every call is argv — no shell, ever — and no flag that
//! weakens verification is reachable from tool arguments.

use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::process::Command;

use crate::policy::valid_package_id;

#[derive(Debug)]
pub enum BrewError {
    Missing(String),
    Failed(String),
    Timeout(String),
}

fn brew_path() -> Option<&'static str> {
    ["/opt/homebrew/bin/brew", "/usr/local/bin/brew"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
}

/// Run `brew` with a hard timeout and a scrubbed environment.
pub async fn brew(args: &[&str], timeout_secs: u64) -> Result<String, BrewError> {
    let Some(path) = brew_path() else {
        return Err(BrewError::Missing(
            "Homebrew is not installed (looked in /opt/homebrew/bin and /usr/local/bin)".into(),
        ));
    };
    // Nothing here may auto-update mid-call or open an interactive prompt: an
    // install that stops to ask a question would hang the session.
    let child = Command::new(path)
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin")
        .env("HOME", std::env::var("HOME").unwrap_or_default())
        .env("HOMEBREW_NO_AUTO_UPDATE", "1")
        .env("HOMEBREW_NO_ANALYTICS", "1")
        .env("HOMEBREW_NO_INSTALL_CLEANUP", "1")
        .env("HOMEBREW_NO_ENV_HINTS", "1")
        .env("NONINTERACTIVE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();

    let out = match tokio::time::timeout(Duration::from_secs(timeout_secs), child).await {
        Err(_) => {
            return Err(BrewError::Timeout(format!(
                "brew {} timed out",
                args.join(" ")
            )))
        }
        Ok(Err(e)) => return Err(BrewError::Failed(format!("brew: {e}"))),
        Ok(Ok(o)) => o,
    };
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(BrewError::Failed(if stderr.is_empty() {
            format!("brew {} failed", args.join(" "))
        } else {
            stderr
        }));
    }
    Ok(stdout)
}

/// Is `id` a cask (an application bundle) rather than a formula?
pub async fn is_cask(id: &str, timeout_secs: u64) -> bool {
    brew(&["info", "--cask", id], timeout_secs).await.is_ok()
}

/// `brew info --json=v2` for one package, as structured JSON.
pub async fn info_json(id: &str, cask: bool, timeout_secs: u64) -> Result<Value, BrewError> {
    let args: Vec<&str> = if cask {
        vec!["info", "--json=v2", "--cask", id]
    } else {
        vec!["info", "--json=v2", "--formula", id]
    };
    let text = brew(&args, timeout_secs).await?;
    serde_json::from_str(&text).map_err(|e| BrewError::Failed(format!("brew info json: {e}")))
}

/// Normalize `brew info --json=v2` into the shape §5.12 asks for.
pub fn summarize(v: &Value, id: &str, cask: bool) -> Value {
    let entry = if cask {
        v.get("casks")
            .and_then(|a| a.as_array())
            .and_then(|a| a.first())
    } else {
        v.get("formulae")
            .and_then(|a| a.as_array())
            .and_then(|a| a.first())
    };
    let Some(e) = entry else {
        return json!({ "id": id, "found": false });
    };
    let version = if cask {
        e.get("version").cloned().unwrap_or(Value::Null)
    } else {
        e.pointer("/versions/stable")
            .cloned()
            .unwrap_or(Value::Null)
    };
    let deps = e
        .get("dependencies")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let installed_version = if cask {
        e.get("installed").cloned().unwrap_or(Value::Null)
    } else {
        e.pointer("/installed/0/version")
            .cloned()
            .unwrap_or(Value::Null)
    };
    json!({
        "id": e.get("full_name").or_else(|| e.get("full_token")).or_else(|| e.get("token"))
              .cloned().unwrap_or_else(|| json!(id)),
        "found": true,
        "name": e.get("name").cloned().unwrap_or(Value::Null),
        "kind": if cask { "cask" } else { "formula" },
        "version": version,
        "installed_version": installed_version,
        "installed": !installed_version.is_null(),
        "description": e.get("desc").cloned().unwrap_or(Value::Null),
        "homepage": e.get("homepage").cloned().unwrap_or(Value::Null),
        "license": e.get("license").cloned().unwrap_or(Value::Null),
        "dependencies": deps,
        "deprecated": e.get("deprecated").cloned().unwrap_or(json!(false)),
    })
}

/// Transitive dependency closure, straight from the manager.
pub async fn deps(id: &str, timeout_secs: u64) -> Result<Vec<String>, BrewError> {
    let text = brew(&["deps", "--include-requirements", id], timeout_secs).await?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && valid_package_id(l))
        .map(str::to_string)
        .collect())
}

/// Installed formulae and casks with versions.
pub async fn installed(timeout_secs: u64) -> Result<Vec<Value>, BrewError> {
    let mut out = Vec::new();
    for (args, kind) in [
        (vec!["list", "--formula", "--versions"], "formula"),
        (vec!["list", "--cask", "--versions"], "cask"),
    ] {
        let Ok(text) = brew(&args, timeout_secs).await else {
            continue;
        };
        for line in text.lines() {
            let mut parts = line.split_whitespace();
            let Some(name) = parts.next() else { continue };
            out.push(json!({
                "id": name,
                "kind": kind,
                "versions": parts.collect::<Vec<_>>(),
                "source": "brew",
            }));
        }
    }
    Ok(out)
}
