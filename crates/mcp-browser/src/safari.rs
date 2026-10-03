//! Native Safari (WebKit) WebDriver support for macOS.
//!
//! Provides multi-engine browser automation alongside Chromium CDP by interfacing
//! with Apple's native `/usr/bin/safaridriver` using the standard W3C WebDriver protocol.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};

use crate::backend::BrowserError;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// Known macOS system locations for Apple's native `safaridriver`.
pub const SAFARI_DRIVER_BINS: &[&str] = &[
    "/usr/bin/safaridriver",
    "/System/Cryptexes/App/usr/bin/safaridriver",
];

/// Discover the path to `safaridriver` on the host machine.
pub fn find_safaridriver() -> Option<PathBuf> {
    for bin in SAFARI_DRIVER_BINS {
        let p = Path::new(bin);
        if p.exists() {
            return Some(p.to_path_buf());
        }
    }
    // Fallback: check PATH
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join("safaridriver");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Check if Safari WebDriver automation is supported on this platform.
pub fn is_safari_available() -> bool {
    cfg!(target_os = "macos") && find_safaridriver().is_some()
}

/// Pick an available free TCP port on loopback.
pub fn pick_free_port() -> Result<u16, BrowserError> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).map_err(|e| {
        BrowserError::Failed(format!("could not bind free port for safaridriver: {e}"))
    })?;
    listener
        .local_addr()
        .map(|a| a.port())
        .map_err(|e| BrowserError::Failed(format!("could not read allocated port: {e}")))
}

/// Configuration options for launching `safaridriver`.
#[derive(Debug, Clone, Default)]
pub struct SafariDriverConfig {
    pub port: Option<u16>,
    pub diagnose: bool,
}

/// Running `safaridriver` process handle.
pub struct SafariProcess {
    pub port: u16,
    child: Option<Child>,
}

impl SafariProcess {
    /// Launch a dedicated `safaridriver` server instance.
    pub async fn launch(config: &SafariDriverConfig) -> Result<Self, BrowserError> {
        let bin = find_safaridriver().ok_or_else(|| {
            BrowserError::NotFound(
                "safaridriver not found. Safari automation is only available on macOS systems with Safari installed."
                    .into(),
            )
        })?;

        // An explicit port is the caller's choice and is never retried. An
        // auto-picked one is released before safaridriver binds it, so another
        // process can win the race; that shows up as our child exiting, and
        // we retry on a fresh port.
        let attempts = if config.port.is_some() { 1 } else { 3 };
        let mut last = BrowserError::Failed("safaridriver did not start".into());
        for _ in 0..attempts {
            let port = match config.port {
                Some(p) => p,
                None => pick_free_port()?,
            };
            match Self::spawn_on(&bin, port, config.diagnose).await {
                Ok(proc) => return Ok(proc),
                Err(e @ BrowserError::Failed(_)) => last = e,
                Err(e) => return Err(e),
            }
        }
        Err(last)
    }

    /// Spawn on one port and wait until *our* child answers `/status`.
    ///
    /// The child must still be running both before and after the answering
    /// `/status`, so a foreign listener that happens to hold the port (our
    /// child having failed to bind and exited) is not mistaken for it.
    async fn spawn_on(bin: &Path, port: u16, diagnose: bool) -> Result<Self, BrowserError> {
        let mut cmd = Command::new(bin);
        cmd.arg("-p").arg(port.to_string());
        if diagnose {
            cmd.arg("--diagnose");
        }
        cmd.stdout(Stdio::null()).stderr(Stdio::null());

        let child = cmd.spawn().map_err(|e| {
            BrowserError::Failed(format!(
                "failed to spawn safaridriver at {}: {e}",
                bin.display()
            ))
        })?;
        // From here `Drop` reaps the child on every early return.
        let mut proc = Self {
            port,
            child: Some(child),
        };

        for _ in 0..30 {
            if let Some(status) = proc.exited() {
                return Err(BrowserError::Failed(format!(
                    "safaridriver exited early on port {port} ({status}); the port may be in use"
                )));
            }
            if let Ok(res) = webdriver_request("127.0.0.1", port, "GET", "/status", None).await {
                let ready = res
                    .get("value")
                    .and_then(|v| v.get("ready"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if ready {
                    // Re-check: the responder must not be a stranger on a
                    // port our child failed to bind.
                    if let Some(status) = proc.exited() {
                        return Err(BrowserError::Failed(format!(
                            "port {port} answered /status but safaridriver exited ({status})"
                        )));
                    }
                    return Ok(proc);
                }
            }
            sleep(Duration::from_millis(100)).await;
        }

        Err(BrowserError::Timeout(format!(
            "safaridriver launched on port {port} but failed to signal readiness within 3s"
        )))
    }

    /// `Some(status)` once the child has exited.
    fn exited(&mut self) -> Option<std::process::ExitStatus> {
        match self.child.as_mut()?.try_wait() {
            Ok(Some(status)) => Some(status),
            _ => None,
        }
    }

    /// Kill the managed `safaridriver` process.
    pub fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for SafariProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Active W3C WebDriver session with Safari.
#[derive(Debug, Clone)]
pub struct SafariSession {
    pub session_id: String,
    pub port: u16,
    pub host: String,
}

impl SafariSession {
    /// Create a new session via `POST /session`. Navigation is deliberately
    /// not done here: the caller owns the navigation policy check.
    pub async fn create(port: u16) -> Result<Self, BrowserError> {
        let payload = json!({
            "capabilities": {
                "alwaysMatch": {
                    "browserName": "safari"
                }
            }
        });

        let resp = webdriver_request("127.0.0.1", port, "POST", "/session", Some(&payload)).await?;

        let session_id = resp
            .get("value")
            .and_then(|v| v.get("sessionId"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                let err_msg = resp
                    .get("value")
                    .and_then(|v| v.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                BrowserError::Failed(format!("failed to create Safari session: {err_msg}"))
            })?
            .to_string();

        Ok(Self {
            session_id,
            port,
            host: "127.0.0.1".into(),
        })
    }

    /// Navigate to a URL via `POST /session/{id}/url`.
    pub async fn navigate(&self, url: &str) -> Result<Value, BrowserError> {
        let payload = json!({ "url": url });
        webdriver_request(
            &self.host,
            self.port,
            "POST",
            &format!("/session/{}/url", self.session_id),
            Some(&payload),
        )
        .await
    }

    /// Get current page URL via `GET /session/{id}/url`.
    pub async fn get_url(&self) -> Result<String, BrowserError> {
        let res = webdriver_request(
            &self.host,
            self.port,
            "GET",
            &format!("/session/{}/url", self.session_id),
            None,
        )
        .await?;
        res.get("value")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| BrowserError::Failed("missing url in response".into()))
    }

    /// Get current page title via `GET /session/{id}/title`.
    pub async fn get_title(&self) -> Result<String, BrowserError> {
        let res = webdriver_request(
            &self.host,
            self.port,
            "GET",
            &format!("/session/{}/title", self.session_id),
            None,
        )
        .await?;
        res.get("value")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| BrowserError::Failed("missing title in response".into()))
    }

    /// Navigate back in browser history.
    pub async fn back(&self) -> Result<Value, BrowserError> {
        webdriver_request(
            &self.host,
            self.port,
            "POST",
            &format!("/session/{}/back", self.session_id),
            Some(&json!({})),
        )
        .await
    }

    /// Navigate forward in browser history.
    pub async fn forward(&self) -> Result<Value, BrowserError> {
        webdriver_request(
            &self.host,
            self.port,
            "POST",
            &format!("/session/{}/forward", self.session_id),
            Some(&json!({})),
        )
        .await
    }

    /// Refresh active page.
    pub async fn refresh(&self) -> Result<Value, BrowserError> {
        webdriver_request(
            &self.host,
            self.port,
            "POST",
            &format!("/session/{}/refresh", self.session_id),
            Some(&json!({})),
        )
        .await
    }

    /// Execute synchronous JavaScript in the page via `POST /session/{id}/execute/sync`.
    pub async fn execute_sync(&self, script: &str, args: &[Value]) -> Result<Value, BrowserError> {
        let payload = json!({
            "script": script,
            "args": args
        });
        let res = webdriver_request(
            &self.host,
            self.port,
            "POST",
            &format!("/session/{}/execute/sync", self.session_id),
            Some(&payload),
        )
        .await?;
        Ok(res.get("value").cloned().unwrap_or(Value::Null))
    }

    /// Execute asynchronous JavaScript via `POST /session/{id}/execute/async`.
    /// The script receives the completion callback as its last argument.
    pub async fn execute_async(&self, script: &str, args: &[Value]) -> Result<Value, BrowserError> {
        let payload = json!({
            "script": script,
            "args": args
        });
        let res = webdriver_request(
            &self.host,
            self.port,
            "POST",
            &format!("/session/{}/execute/async", self.session_id),
            Some(&payload),
        )
        .await?;
        Ok(res.get("value").cloned().unwrap_or(Value::Null))
    }

    /// Evaluate an expression that may be a Promise (for example an
    /// `(async function(){...})()` IIFE) and return what it settles to.
    ///
    /// `execute/sync` does not await promises, so such a script would come back
    /// as `{}` and look like success. A rejection or a synchronous throw is
    /// reported as `{ok:false,error}`.
    pub async fn eval_promise(&self, expr: &str) -> Result<Value, BrowserError> {
        self.execute_async(&promise_script(expr), &[]).await
    }

    /// Capture page screenshot via `GET /session/{id}/screenshot` returning base64 PNG.
    pub async fn screenshot(&self) -> Result<String, BrowserError> {
        let res = webdriver_request(
            &self.host,
            self.port,
            "GET",
            &format!("/session/{}/screenshot", self.session_id),
            None,
        )
        .await?;
        res.get("value")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| BrowserError::Failed("missing screenshot data in response".into()))
    }

    /// Capture one element via `GET /session/{id}/element/{eid}/screenshot`.
    /// `element` is the W3C element reference object a script returned.
    pub async fn element_screenshot(&self, element: &Value) -> Result<String, BrowserError> {
        let id = element_id(element).ok_or_else(|| {
            BrowserError::Failed("script did not return a WebDriver element reference".into())
        })?;
        let res = webdriver_request(
            &self.host,
            self.port,
            "GET",
            &format!("/session/{}/element/{id}/screenshot", self.session_id),
            None,
        )
        .await?;
        res.get("value")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| BrowserError::Failed("missing screenshot data in response".into()))
    }

    /// Text of the open alert, or `None` when no alert is open.
    pub async fn alert_text(&self) -> Result<Option<String>, BrowserError> {
        match webdriver_request(
            &self.host,
            self.port,
            "GET",
            &format!("/session/{}/alert/text", self.session_id),
            None,
        )
        .await
        {
            Ok(res) => Ok(Some(
                res.get("value")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            )),
            Err(BrowserError::Failed(m)) if is_no_such_alert(&m) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Type into an open `prompt()` via `POST /session/{id}/alert/text`.
    pub async fn send_alert_text(&self, text: &str) -> Result<(), BrowserError> {
        webdriver_request(
            &self.host,
            self.port,
            "POST",
            &format!("/session/{}/alert/text", self.session_id),
            Some(&json!({ "text": text })),
        )
        .await?;
        Ok(())
    }

    /// Get all cookies via `GET /session/{id}/cookie`.
    pub async fn get_cookies(&self) -> Result<Vec<Value>, BrowserError> {
        let res = webdriver_request(
            &self.host,
            self.port,
            "GET",
            &format!("/session/{}/cookie", self.session_id),
            None,
        )
        .await?;
        res.get("value")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| BrowserError::Failed("missing cookie list in response".into()))
    }

    /// Add a cookie via `POST /session/{id}/cookie`.
    pub async fn add_cookie(&self, cookie: &Value) -> Result<(), BrowserError> {
        let payload = json!({ "cookie": cookie });
        webdriver_request(
            &self.host,
            self.port,
            "POST",
            &format!("/session/{}/cookie", self.session_id),
            Some(&payload),
        )
        .await?;
        Ok(())
    }

    /// Delete all cookies via `DELETE /session/{id}/cookie`.
    pub async fn delete_cookies(&self) -> Result<(), BrowserError> {
        webdriver_request(
            &self.host,
            self.port,
            "DELETE",
            &format!("/session/{}/cookie", self.session_id),
            None,
        )
        .await?;
        Ok(())
    }

    /// Set window rect via `POST /session/{id}/window/rect`.
    pub async fn set_window_rect(&self, width: u32, height: u32) -> Result<Value, BrowserError> {
        let payload = json!({ "width": width, "height": height });
        webdriver_request(
            &self.host,
            self.port,
            "POST",
            &format!("/session/{}/window/rect", self.session_id),
            Some(&payload),
        )
        .await
    }

    /// Accept active dialog/alert via `POST /session/{id}/alert/accept`.
    pub async fn accept_alert(&self) -> Result<(), BrowserError> {
        webdriver_request(
            &self.host,
            self.port,
            "POST",
            &format!("/session/{}/alert/accept", self.session_id),
            Some(&json!({})),
        )
        .await?;
        Ok(())
    }

    /// Dismiss active dialog/alert via `POST /session/{id}/alert/dismiss`.
    pub async fn dismiss_alert(&self) -> Result<(), BrowserError> {
        webdriver_request(
            &self.host,
            self.port,
            "POST",
            &format!("/session/{}/alert/dismiss", self.session_id),
            Some(&json!({})),
        )
        .await?;
        Ok(())
    }

    /// Close and delete session via `DELETE /session/{id}`.
    pub async fn close(&self) -> Result<(), BrowserError> {
        let _ = webdriver_request(
            &self.host,
            self.port,
            "DELETE",
            &format!("/session/{}", self.session_id),
            None,
        )
        .await;
        Ok(())
    }
}

/// The W3C key under which a script-returned element carries its id.
const ELEMENT_KEY: &str = "element-6066-11e4-a52e-4f735466cecf";

/// Largest response body accepted. Screenshots are the big ones (base64 PNG).
const MAX_BODY: usize = 64 * 1024 * 1024;

fn element_id(v: &Value) -> Option<&str> {
    v.get(ELEMENT_KEY).and_then(Value::as_str)
}

fn is_no_such_alert(msg: &str) -> bool {
    msg.contains("no such alert")
}

/// Wrap a promise-returning expression for `execute/async`.
fn promise_script(expr: &str) -> String {
    format!(
        "var done = arguments[arguments.length - 1];\n\
         Promise.resolve().then(function(){{ return {expr}; }})\n\
         .then(done, function(e){{ done({{ok:false,error:String(e)}}); }});"
    )
}

/// Issue an HTTP request to a W3C WebDriver endpoint and parse the JSON reply.
pub async fn webdriver_request(
    host: &str,
    port: u16,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<Value, BrowserError> {
    let connect_future = TcpStream::connect((host, port));
    let mut stream = timeout(DEFAULT_TIMEOUT, connect_future)
        .await
        .map_err(|_| {
            BrowserError::Timeout(format!(
                "timeout connecting to safaridriver at {host}:{port}"
            ))
        })?
        .map_err(|e| BrowserError::Failed(format!("connect {host}:{port}: {e}")))?;

    let body_bytes = match body {
        Some(b) => {
            serde_json::to_vec(b).map_err(|e| BrowserError::Failed(format!("json encode: {e}")))?
        }
        None => Vec::new(),
    };

    let req_headers = format!(
        "{method} {path} HTTP/1.1\r\n\
         Host: {host}:{port}\r\n\
         User-Agent: agentctl/0.1.0\r\n\
         Accept: application/json\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body_bytes.len()
    );

    stream
        .write_all(req_headers.as_bytes())
        .await
        .map_err(|e| BrowserError::Failed(format!("write request headers: {e}")))?;

    if !body_bytes.is_empty() {
        stream
            .write_all(&body_bytes)
            .await
            .map_err(|e| BrowserError::Failed(format!("write request body: {e}")))?;
    }

    let mut resp_buf = Vec::new();
    // Bounded: a misbehaving peer must not exhaust memory.
    let mut limited = (&mut stream).take(MAX_BODY as u64 + 1);
    let read_future = limited.read_to_end(&mut resp_buf);
    timeout(DEFAULT_TIMEOUT, read_future)
        .await
        .map_err(|_| BrowserError::Timeout("timeout reading safaridriver response".into()))?
        .map_err(|e| BrowserError::Failed(format!("read response: {e}")))?;

    if resp_buf.len() > MAX_BODY {
        return Err(BrowserError::Failed(format!(
            "safaridriver response exceeds {MAX_BODY} bytes"
        )));
    }
    parse_http_response(&resp_buf)
}

/// Decode a `Transfer-Encoding: chunked` body.
fn decode_chunked(mut b: &[u8]) -> Result<Vec<u8>, BrowserError> {
    let bad = |m: &str| BrowserError::Failed(format!("malformed chunked response: {m}"));
    let mut out = Vec::new();
    loop {
        let eol = b
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| bad("missing chunk size line"))?;
        let line = std::str::from_utf8(&b[..eol]).map_err(|_| bad("non-utf8 chunk size"))?;
        let size_hex = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16).map_err(|_| bad("bad chunk size"))?;
        b = &b[eol + 2..];
        if size == 0 {
            return Ok(out);
        }
        if b.len() < size + 2 {
            return Err(bad("truncated chunk"));
        }
        out.extend_from_slice(&b[..size]);
        b = &b[size + 2..];
    }
}

fn parse_http_response(buf: &[u8]) -> Result<Value, BrowserError> {
    let sep = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| {
            BrowserError::Failed(
                "malformed HTTP response from safaridriver (no header boundary)".into(),
            )
        })?;

    let header_str = String::from_utf8_lossy(&buf[..sep]);
    let status_code = header_str
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(500);

    let raw_body = &buf[sep + 4..];
    let lower = header_str.to_ascii_lowercase();
    let chunked = lower
        .lines()
        .any(|l| l.starts_with("transfer-encoding:") && l.contains("chunked"));
    let decoded;
    let body_bytes: &[u8] = if chunked {
        decoded = decode_chunked(raw_body)?;
        &decoded
    } else {
        if let Some(len) = lower
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse::<usize>().ok())
        {
            if raw_body.len() < len {
                return Err(BrowserError::Failed(format!(
                    "truncated response from safaridriver ({} of {len} bytes)",
                    raw_body.len()
                )));
            }
        }
        raw_body
    };
    // An unparseable body is only tolerable on an error status, where the
    // status code already says the call failed.
    let parsed: Result<Value, _> = serde_json::from_slice(body_bytes);
    if parsed.is_err() && status_code < 400 {
        return Err(BrowserError::Failed(format!(
            "unparseable JSON in HTTP {status_code} response from safaridriver"
        )));
    }
    let val = parsed.unwrap_or(Value::Null);

    if status_code >= 400 {
        let err_msg = val
            .get("value")
            .and_then(|v| v.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("WebDriver command failed");
        let err_code = val
            .get("value")
            .and_then(|v| v.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("");

        if err_msg.contains("Allow remote automation") || err_msg.contains("remote automation") {
            return Err(BrowserError::PermissionDenied(
                "Safari remote automation is disabled. To enable it on macOS:\n\
                 1. Open Safari > Settings (or Preferences) > Advanced\n\
                 2. Check 'Show features for web developers' (or 'Show Develop menu in menu bar')\n\
                 3. Click the 'Develop' menu in the macOS menu bar > check 'Allow Remote Automation'\n\
                 4. Or run 'safaridriver --enable' in Terminal and re-run your command."
                    .into(),
            ));
        }

        return Err(BrowserError::Failed(format!(
            "HTTP {status_code}: {err_code}: {err_msg}"
        )));
    }

    Ok(val)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Discovery must only report a path that exists. Whether Safari is
    /// installed is a property of the machine, so absence is not a failure.
    #[test]
    fn test_safaridriver_binary_discovery() {
        if !cfg!(target_os = "macos") {
            return;
        }
        if let Some(p) = find_safaridriver() {
            assert!(p.exists());
        }
    }

    #[test]
    fn test_pick_free_port() {
        let p1 = pick_free_port().expect("free port 1");
        let p2 = pick_free_port().expect("free port 2");
        assert!(p1 > 1024);
        assert!(p2 > 1024);
    }

    #[test]
    fn test_parse_remote_automation_error() {
        let raw_500 = b"HTTP/1.1 500 Internal Server Error\r\n\
Content-Type: application/json\r\n\
Content-Length: 174\r\n\r\n\
{\"value\":{\"error\":\"session not created\",\"message\":\"Could not create a session: You must enable 'Allow remote automation' in the Developer section of Safari Settings to control Safari via WebDriver.\",\"stacktrace\":\"\"}}";

        let err = parse_http_response(raw_500).unwrap_err();
        match err {
            BrowserError::PermissionDenied(msg) => {
                assert!(msg.contains("Allow Remote Automation"));
                assert!(msg.contains("safaridriver --enable"));
            }
            other => panic!("expected PermissionDenied error, got: {other:?}"),
        }
    }

    #[test]
    fn test_parse_success_response() {
        let raw_200 = b"HTTP/1.1 200 OK\r\n\
Content-Type: application/json\r\n\
Content-Length: 35\r\n\r\n\
{\"value\":{\"message\":\"\",\"ready\":true}}";

        let val = parse_http_response(raw_200).expect("parse ok");
        assert_eq!(val["value"]["ready"], true);
    }

    #[test]
    fn unparseable_success_body_is_an_error() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\nnot json";
        assert!(matches!(
            parse_http_response(raw),
            Err(BrowserError::Failed(m)) if m.contains("unparseable")
        ));
    }

    #[test]
    fn truncated_body_is_an_error() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 50\r\n\r\n{\"value\":1}";
        assert!(matches!(
            parse_http_response(raw),
            Err(BrowserError::Failed(_))
        ));
    }

    #[test]
    fn chunked_body_is_decoded() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
8\r\n{\"value\"\r\n5\r\n:42}\n\r\n0\r\n\r\n";
        let v = parse_http_response(raw).expect("chunked parses");
        assert_eq!(v["value"], 42);
    }

    #[test]
    fn malformed_chunk_is_an_error() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n";
        assert!(parse_http_response(raw).is_err());
        let short = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nA\r\nabc\r\n";
        assert!(parse_http_response(short).is_err());
    }

    #[test]
    fn no_such_alert_is_recognised_from_the_error_message() {
        let raw = b"HTTP/1.1 404 Not Found\r\n\r\n{\"value\":{\"error\":\"no such alert\",\"message\":\"\"}}";
        match parse_http_response(raw) {
            Err(BrowserError::Failed(m)) => assert!(is_no_such_alert(&m), "{m}"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn promise_script_uses_the_callback_and_reports_rejection() {
        let s = promise_script("(async function(){ return 1; })()");
        assert!(s.contains("arguments[arguments.length - 1]"));
        assert!(s.contains("(async function(){ return 1; })()"));
        assert!(s.contains("ok:false"));
    }

    #[test]
    fn element_reference_is_extracted() {
        let v = json!({ ELEMENT_KEY: "abc-1" });
        assert_eq!(element_id(&v), Some("abc-1"));
        assert_eq!(element_id(&json!({})), None);
    }

    /// An exited child must be detected promptly instead of waiting out the
    /// readiness timeout. `false` stands in for a driver that fails to bind.
    #[tokio::test]
    async fn early_child_exit_is_detected_quickly() {
        let Some(false_bin) = ["/usr/bin/false", "/bin/false"]
            .iter()
            .map(Path::new)
            .find(|p| p.exists())
        else {
            return;
        };
        let started = std::time::Instant::now();
        let err = SafariProcess::spawn_on(false_bin, 1, false)
            .await
            .err()
            .expect("an exited child is not a driver");
        assert!(matches!(err, BrowserError::Failed(m) if m.contains("exited early")));
        assert!(started.elapsed() < Duration::from_millis(2500));
    }
}
