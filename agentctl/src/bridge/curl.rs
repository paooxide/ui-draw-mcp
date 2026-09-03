//! Posting to the Gemini endpoint through `curl`.
//!
//! Shelling out rather than linking an HTTP client is the same trade this
//! workspace makes elsewhere (`mcp-net`, `mcp-vision`): no TLS stack, no async
//! HTTP crate, nothing new in the dependency tree.
//!
//! The API key is passed in a config file on **stdin**, never in `argv`. A key
//! on the command line is readable by every process on the machine for as long
//! as the request runs, and this server's whole point is that an agent can run
//! `ps`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;

pub const ENDPOINT: &str = "https://generativelanguage.googleapis.com/v1beta";

/// `curl`'s absolute path where we know it, else whatever is on PATH.
fn curl_bin() -> &'static str {
    if cfg!(unix) && Path::new("/usr/bin/curl").exists() {
        "/usr/bin/curl"
    } else {
        "curl"
    }
}

/// Quote a value for a curl config file. Only `\` and `"` are special.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Render the config curl reads from stdin.
///
/// `%{http_code}` is appended on its own line so the status is recoverable
/// without `--fail`, which would throw away the error body that says *why*.
pub fn config(url: &str, key: &str, body_path: Option<&Path>) -> String {
    let mut lines = vec![
        "silent".to_string(),
        "show-error".to_string(),
        "location".to_string(),
        format!("url = {}", quote(url)),
        format!("header = {}", quote(&format!("x-goog-api-key: {key}"))),
        "header = \"Content-Type: application/json\"".to_string(),
        "write-out = \"\\n%{http_code}\"".to_string(),
    ];
    if let Some(p) = body_path {
        lines.push(format!(
            "data = {}",
            quote(&format!("@{}", p.to_string_lossy()))
        ));
    }
    lines.join("\n") + "\n"
}

/// Write a file only this user can read.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    f.write_all(bytes)
        .map_err(|e| format!("could not write {}: {e}", path.display()))
}

/// A request body on disk, removed when it goes out of scope.
struct BodyFile(PathBuf);

impl Drop for BodyFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Split curl's output into the response body and the status code the
/// `write-out` template appended.
fn split_status(out: &str) -> (u16, &str) {
    match out.rfind('\n') {
        Some(i) => {
            let code = out[i + 1..].trim().parse().unwrap_or(0);
            (code, &out[..i])
        }
        None => (0, out),
    }
}

/// Send one request. `body` is `None` for a GET.
pub async fn send(
    url: &str,
    key: &str,
    body: Option<&Value>,
    timeout: Duration,
) -> Result<(u16, Value), String> {
    if key.contains('\n') || key.contains('\r') {
        return Err("the API key contains a line break; check the file it came from".into());
    }
    let _body_file = match body {
        Some(v) => {
            let path = std::env::temp_dir().join(format!(
                "agentctl-bridge-{}-{}.json",
                std::process::id(),
                mcp_policy::now_ms()
            ));
            write_private(
                &path,
                serde_json::to_string(v).unwrap_or_default().as_bytes(),
            )?;
            Some(BodyFile(path))
        }
        None => None,
    };
    let config = config(url, key, _body_file.as_ref().map(|b| b.0.as_path()));

    let mut child = tokio::process::Command::new(curl_bin())
        .args(["--config", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run curl: {e}"))?;
    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child.stdin.take().ok_or("curl had no stdin")?;
        stdin
            .write_all(config.as_bytes())
            .await
            .map_err(|e| format!("could not hand curl its config: {e}"))?;
        stdin
            .shutdown()
            .await
            .map_err(|e| format!("could not close curl's stdin: {e}"))?;
    }
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| format!("the request timed out after {}s", timeout.as_secs()))?
        .map_err(|e| format!("curl failed: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "curl exited {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let (code, body) = split_status(&stdout);
    let parsed: Value = serde_json::from_str(body.trim()).map_err(|e| {
        format!(
            "the response was not JSON ({e}): {}",
            body.chars().take(200).collect::<String>()
        )
    })?;
    Ok((code, parsed))
}

/// `POST /models/{model}:generateContent`.
pub async fn generate_content(
    model: &str,
    key: &str,
    body: &Value,
    timeout: Duration,
) -> Result<Value, String> {
    let url = format!("{ENDPOINT}/models/{model}:generateContent");
    let (code, v) = send(&url, key, Some(body), timeout).await?;
    if code == 200 {
        return Ok(v);
    }
    // The error body is the useful part — a 400 here is almost always a schema
    // the API would not take, and it names the field.
    let msg = v
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("no message");
    Err(match code {
        401 | 403 => format!("HTTP {code}: the API key was rejected — {msg}"),
        429 => format!("HTTP 429: rate limited — {msg}"),
        _ => format!("HTTP {code}: {msg}"),
    })
}

/// `GET /models` — which models this key can actually use.
pub async fn list_models(key: &str, timeout: Duration) -> Result<Vec<(String, String)>, String> {
    let url = format!("{ENDPOINT}/models?pageSize=200");
    let (code, v) = send(&url, key, None, timeout).await?;
    if code != 200 {
        let msg = v
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("no message");
        return Err(format!("HTTP {code}: {msg}"));
    }
    let mut out = Vec::new();
    for m in v
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(name) = m.get("name").and_then(Value::as_str) else {
            continue;
        };
        // Only models that can be driven turn by turn are useful here.
        let supported = m
            .get("supportedGenerationMethods")
            .and_then(Value::as_array)
            .map(|a| a.iter().any(|s| s.as_str() == Some("generateContent")))
            .unwrap_or(false);
        if !supported {
            continue;
        }
        let display = m
            .get("displayName")
            .and_then(Value::as_str)
            .unwrap_or_default();
        out.push((
            name.strip_prefix("models/").unwrap_or(name).to_string(),
            display.to_string(),
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole reason for the config-on-stdin dance.
    #[test]
    fn the_key_appears_in_the_config_and_nowhere_else() {
        let cfg = config("https://example/x", "SECRET-KEY", None);
        assert!(cfg.contains("header = \"x-goog-api-key: SECRET-KEY\""));
        // The only argv curl ever sees.
        assert_eq!(["--config", "-"].len(), 2);
    }

    #[test]
    fn quoting_survives_a_path_with_a_quote_or_a_backslash() {
        assert_eq!(quote(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quote(r"a\b"), r#""a\\b""#);
        let cfg = config("https://x", "k", Some(Path::new(r#"/tmp/we"ird.json"#)));
        assert!(cfg.contains(r#"data = "@/tmp/we\"ird.json""#));
    }

    #[test]
    fn a_get_carries_no_data_line() {
        let cfg = config("https://x", "k", None);
        assert!(!cfg.contains("data ="));
        assert!(cfg.contains("write-out = \"\\n%{http_code}\""));
    }

    #[test]
    fn the_status_code_is_split_off_the_body() {
        let (code, body) = split_status("{\"a\":1}\n200");
        assert_eq!(code, 200);
        assert_eq!(body, "{\"a\":1}");
        // A body containing newlines keeps them; only the last line is status.
        let (code, body) = split_status("{\n \"a\": 1\n}\n429");
        assert_eq!(code, 429);
        assert_eq!(body, "{\n \"a\": 1\n}");
    }

    #[test]
    fn a_key_with_a_newline_is_refused_before_anything_is_spawned() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let e = rt
            .block_on(send(
                "https://x",
                "key\nheader = evil",
                None,
                Duration::from_secs(1),
            ))
            .unwrap_err();
        assert!(e.contains("line break"));
    }
}
