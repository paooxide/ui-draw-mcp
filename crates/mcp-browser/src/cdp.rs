//! Minimal Chrome DevTools Protocol transport, hand-rolled to avoid a heavy
//! WebSocket/HTTP dependency (disk-conscious, see `docs/planning.md` §12).
//!
//! Two pieces:
//!   * [`http_json`] — talks to Chrome's `/json/*` HTTP endpoints (target
//!     discovery/lifecycle). Chrome keeps the socket alive, so it reads headers
//!     then exactly `Content-Length` body bytes rather than to EOF.
//!   * [`CdpConn`] — a single RFC 6455 client WebSocket to one target's
//!     `webSocketDebuggerUrl`, with a synchronous request/response `call`
//!     (events between our command and its reply are drained and ignored).
//!
//! CDP sessions are per-connection but page state lives in the browser, so we
//! open a fresh connection per tool call — cheap on localhost and avoids a
//! background read-pump.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};

use crate::backend::BrowserError;

const CALL_TIMEOUT: Duration = Duration::from_secs(20);

fn io_fail(e: std::io::Error) -> BrowserError {
    BrowserError::Failed(format!("io: {e}"))
}

/// A rough process/time seed for WebSocket masking + key generation. These do
/// not need to be cryptographically strong — masking only guards against
/// broken proxies, and localhost CDP has no proxy.
fn seed() -> u64 {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    t ^ ((std::process::id() as u64) << 17) ^ 0x9E37_79B9_7F4A_7C15
}

/// Standard base64 with padding (used only for the `Sec-WebSocket-Key`).
fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn parse_status(head: &[u8]) -> Result<u16, BrowserError> {
    let line = head
        .split(|&b| b == b'\r' || b == b'\n')
        .next()
        .ok_or_else(|| BrowserError::Failed("empty http response".into()))?;
    let s = String::from_utf8_lossy(line);
    s.split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| BrowserError::Failed(format!("bad http status line: {s}")))
}

fn find_sep(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Extract the `Content-Length` header value (case-insensitive) from raw
/// response headers.
fn content_length(head: &[u8]) -> Option<usize> {
    let s = String::from_utf8_lossy(head);
    for line in s.split("\r\n") {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                return v.trim().parse::<usize>().ok();
            }
        }
    }
    None
}

/// Issue a request to a Chrome `/json/*` endpoint and parse the JSON body.
/// `method` is usually `GET`; `/json/new` needs `PUT` on modern Chrome.
///
/// Chrome's DevTools HTTP server keeps the connection alive and ignores
/// `Connection: close`, so we cannot read to EOF — we read headers, then
/// exactly `Content-Length` body bytes (falling back to read-to-EOF only when
/// the header is absent).
pub async fn http_json(
    host: &str,
    port: u16,
    method: &str,
    path: &str,
) -> Result<Value, BrowserError> {
    let mut s = TcpStream::connect((host, port))
        .await
        .map_err(|e| BrowserError::Failed(format!("connect {host}:{port}: {e}")))?;
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nAccept: application/json\r\n\r\n"
    );
    s.write_all(req.as_bytes()).await.map_err(io_fail)?;

    // Read until we have the full header block.
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let sep = loop {
        if let Some(p) = find_sep(&buf) {
            break p;
        }
        let n = s.read(&mut tmp).await.map_err(io_fail)?;
        if n == 0 {
            return Err(BrowserError::Failed(
                "connection closed before headers".into(),
            ));
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > 65536 {
            return Err(BrowserError::Failed("http headers too large".into()));
        }
    };
    let status = parse_status(&buf[..sep])?;
    if !(200..300).contains(&status) {
        return Err(BrowserError::Failed(format!("HTTP {status} for {path}")));
    }
    let body_start = sep + 4;
    // Read the body: exactly Content-Length bytes, or to EOF if unspecified.
    match content_length(&buf[..sep]) {
        Some(len) => {
            while buf.len() < body_start + len {
                let n = s.read(&mut tmp).await.map_err(io_fail)?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
        }
        None => {
            s.read_to_end(&mut buf).await.map_err(io_fail)?;
        }
    }
    let body = &buf[body_start..];
    if body.is_empty() {
        return Ok(json!({}));
    }
    // activate/close return a plain string ("Target activated"); wrap it.
    Ok(serde_json::from_slice::<Value>(body)
        .unwrap_or_else(|_| json!({ "text": String::from_utf8_lossy(body) })))
}

/// What to do when the page raises a JavaScript dialog while a command is in
/// flight (`alert`, `confirm`, `prompt`, `beforeunload`).
///
/// This is not optional behaviour. Once a CDP client has enabled the `Page`
/// domain, Chrome stops showing the native dialog and hands it to that client
/// instead — so a client that ignores the event leaves the renderer blocked
/// forever, and every later call against the tab times out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum DialogPolicy {
    /// Cancel it: `confirm` yields `false`, `prompt` yields `null`, and
    /// `beforeunload` keeps the page (so unsaved work is not discarded).
    /// The safe default — accepting on the agent's behalf would let a page
    /// turn "are you sure?" into "yes" without anyone deciding.
    #[default]
    Dismiss,
    /// Accept it, optionally supplying text for `prompt`.
    Accept(Option<String>),
}

/// A single client WebSocket to one CDP target.
pub struct CdpConn {
    stream: TcpStream,
    next_id: u64,
    rng: u64,
    dialog_policy: DialogPolicy,
    dialogs: Vec<Value>,
}

impl CdpConn {
    /// Connect and perform the RFC 6455 upgrade handshake. We do not validate
    /// `Sec-WebSocket-Accept` (would need SHA-1) — a `101` status is enough on
    /// a trusted localhost endpoint.
    pub async fn connect(ws_url: &str) -> Result<Self, BrowserError> {
        let rest = ws_url
            .strip_prefix("ws://")
            .ok_or_else(|| BrowserError::Failed(format!("expected ws:// url, got {ws_url}")))?;
        let slash = rest.find('/').unwrap_or(rest.len());
        let authority = &rest[..slash];
        let path = if slash < rest.len() {
            &rest[slash..]
        } else {
            "/"
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (
                h.to_string(),
                p.parse::<u16>()
                    .map_err(|_| BrowserError::Failed("bad ws port".into()))?,
            ),
            None => (authority.to_string(), 80),
        };
        let mut stream = TcpStream::connect((host.as_str(), port))
            .await
            .map_err(|e| BrowserError::Failed(format!("ws connect {host}:{port}: {e}")))?;

        let seed = seed();
        let key = b64(&seed.to_le_bytes().repeat(2)[..16]);
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        stream.write_all(req.as_bytes()).await.map_err(io_fail)?;

        // Read response headers up to the blank line.
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stream.read_exact(&mut byte).await.map_err(io_fail)?;
            head.push(byte[0]);
            if head.ends_with(b"\r\n\r\n") {
                break;
            }
            if head.len() > 8192 {
                return Err(BrowserError::Failed(
                    "ws handshake headers too large".into(),
                ));
            }
        }
        let status = parse_status(&head)?;
        if status != 101 {
            return Err(BrowserError::Failed(format!(
                "ws upgrade rejected: HTTP {status}"
            )));
        }
        Ok(CdpConn {
            stream,
            next_id: 1,
            rng: seed,
            dialog_policy: DialogPolicy::default(),
            dialogs: Vec::new(),
        })
    }

    pub fn set_dialog_policy(&mut self, policy: DialogPolicy) {
        self.dialog_policy = policy;
    }

    /// Take the JavaScript dialogs observed (and answered) since the last call.
    pub fn take_dialogs(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.dialogs)
    }

    /// Answer a `Page.javascriptDialogOpening` event on the connection that saw
    /// it, and record what was asked.
    ///
    /// It has to be this connection: Chrome tracks the pending dialog per
    /// DevTools session, so a session attaching afterwards gets "No dialog is
    /// showing" and the tab stays wedged. The reply is fire-and-forget — its
    /// response is drained by the caller's loop like any other event.
    async fn answer_dialog(&mut self, params: &Value) -> Result<(), BrowserError> {
        let (accept, text) = match &self.dialog_policy {
            DialogPolicy::Dismiss => (false, None),
            DialogPolicy::Accept(t) => (true, t.clone()),
        };
        self.dialogs.push(json!({
            "type": params.get("type").cloned().unwrap_or(Value::Null),
            "message": params.get("message").cloned().unwrap_or(Value::Null),
            "url": params.get("url").cloned().unwrap_or(Value::Null),
            "default_prompt": params.get("defaultPrompt").cloned().unwrap_or(Value::Null),
            "answered": if accept { "accepted" } else { "dismissed" },
        }));
        let id = self.next_id;
        self.next_id += 1;
        let mut p = json!({ "accept": accept });
        if let Some(t) = text {
            p["promptText"] = json!(t);
        }
        let msg = json!({ "id": id, "method": "Page.handleJavaScriptDialog", "params": p });
        self.send_text(&msg.to_string()).await
    }

    fn next_mask(&mut self) -> [u8; 4] {
        // LCG; unpredictability is not required for correctness.
        self.rng = self
            .rng
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let x = self.rng;
        [(x >> 24) as u8, (x >> 16) as u8, (x >> 8) as u8, x as u8]
    }

    async fn send_text(&mut self, text: &str) -> Result<(), BrowserError> {
        let payload = text.as_bytes();
        let len = payload.len();
        let mut frame = Vec::with_capacity(len + 14);
        frame.push(0x81); // FIN + text opcode
        let mask_bit = 0x80u8;
        if len < 126 {
            frame.push(mask_bit | len as u8);
        } else if len <= 0xFFFF {
            frame.push(mask_bit | 126);
            frame.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            frame.push(mask_bit | 127);
            frame.extend_from_slice(&(len as u64).to_be_bytes());
        }
        let mask = self.next_mask();
        frame.extend_from_slice(&mask);
        frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.stream.write_all(&frame).await.map_err(io_fail)?;
        Ok(())
    }

    async fn send_pong(&mut self, payload: &[u8]) -> Result<(), BrowserError> {
        let mut frame = vec![0x8A]; // FIN + pong
        let mask = self.next_mask();
        frame.push(0x80 | payload.len().min(125) as u8);
        frame.extend_from_slice(&mask);
        frame.extend(
            payload
                .iter()
                .take(125)
                .enumerate()
                .map(|(i, b)| b ^ mask[i % 4]),
        );
        self.stream.write_all(&frame).await.map_err(io_fail)?;
        Ok(())
    }

    /// Read one frame: `(fin, opcode, unmasked_payload)`. Server→client frames
    /// are never masked.
    async fn read_frame(&mut self) -> Result<(bool, u8, Vec<u8>), BrowserError> {
        let mut h = [0u8; 2];
        self.stream.read_exact(&mut h).await.map_err(io_fail)?;
        let fin = h[0] & 0x80 != 0;
        let opcode = h[0] & 0x0F;
        let masked = h[1] & 0x80 != 0;
        let mut len = (h[1] & 0x7F) as u64;
        if len == 126 {
            let mut e = [0u8; 2];
            self.stream.read_exact(&mut e).await.map_err(io_fail)?;
            len = u16::from_be_bytes(e) as u64;
        } else if len == 127 {
            let mut e = [0u8; 8];
            self.stream.read_exact(&mut e).await.map_err(io_fail)?;
            len = u64::from_be_bytes(e);
        }
        let mut mask = [0u8; 4];
        if masked {
            self.stream.read_exact(&mut mask).await.map_err(io_fail)?;
        }
        let mut payload = vec![0u8; len as usize];
        self.stream
            .read_exact(&mut payload)
            .await
            .map_err(io_fail)?;
        if masked {
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= mask[i % 4];
            }
        }
        Ok((fin, opcode, payload))
    }

    /// Read one full CDP message (reassembling fragments, answering pings).
    async fn read_message(&mut self) -> Result<Value, BrowserError> {
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let (fin, opcode, payload) = self.read_frame().await?;
            match opcode {
                0x9 => {
                    self.send_pong(&payload).await?;
                    continue;
                }
                0xA => continue,
                0x8 => return Err(BrowserError::Failed("websocket closed by browser".into())),
                _ => {}
            }
            buf.extend_from_slice(&payload);
            if fin {
                break;
            }
        }
        serde_json::from_slice(&buf).map_err(|e| BrowserError::Failed(format!("bad cdp json: {e}")))
    }

    /// Collect events for a bounded window, keeping only `methods`.
    ///
    /// A CDP client that never reads events is not "quiet" — the events queue on
    /// the socket regardless. Draining them into a capped buffer is what makes
    /// request logging possible without an unbounded background reader.
    pub async fn collect_events(
        &mut self,
        methods: &[&str],
        duration_ms: u64,
        max: usize,
    ) -> Result<Vec<Value>, BrowserError> {
        use tokio::time::{Duration, Instant};
        let deadline = Instant::now() + Duration::from_millis(duration_ms);
        let mut out = Vec::new();
        while Instant::now() < deadline && out.len() < max {
            let left = deadline.saturating_duration_since(Instant::now());
            let msg = match timeout(left, self.read_message()).await {
                Err(_) => break,
                Ok(Err(e)) => return Err(e),
                Ok(Ok(v)) => v,
            };
            let Some(method) = msg.get("method").and_then(Value::as_str) else {
                continue;
            };
            if method == "Page.javascriptDialogOpening" {
                let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
                self.answer_dialog(&params).await?;
                continue;
            }
            if methods.contains(&method) {
                out.push(msg);
            }
        }
        Ok(out)
    }

    /// Send a CDP command and return its `result`, draining unrelated events.
    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value, BrowserError> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({ "id": id, "method": method, "params": params }).to_string();
        self.send_text(&msg).await?;
        loop {
            let v = timeout(CALL_TIMEOUT, self.read_message())
                .await
                .map_err(|_| BrowserError::Timeout(format!("cdp {method} timed out")))??;
            if v.get("method").and_then(Value::as_str) == Some("Page.javascriptDialogOpening") {
                let params = v.get("params").cloned().unwrap_or_else(|| json!({}));
                self.answer_dialog(&params).await?;
                continue;
            }
            if v.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(err) = v.get("error") {
                    let m = err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("cdp error");
                    return Err(BrowserError::Failed(format!("{method}: {m}")));
                }
                return Ok(v.get("result").cloned().unwrap_or_else(|| json!({})));
            }
            // otherwise an event or a stale id — keep reading.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(b64(b""), "");
        assert_eq!(b64(b"f"), "Zg==");
        assert_eq!(b64(b"fo"), "Zm8=");
        assert_eq!(b64(b"foo"), "Zm9v");
        assert_eq!(b64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn status_parses_from_head() {
        assert_eq!(
            parse_status(b"HTTP/1.1 101 Switching Protocols\r\n").unwrap(),
            101
        );
        assert_eq!(parse_status(b"HTTP/1.1 200 OK\r\n").unwrap(), 200);
        assert!(parse_status(b"garbage").is_err());
    }

    #[test]
    fn header_separator_found() {
        assert_eq!(find_sep(b"a: b\r\n\r\nBODY"), Some(4));
        assert_eq!(find_sep(b"no terminator"), None);
    }

    #[test]
    fn ws_key_is_16_bytes_base64() {
        let key = b64(&seed().to_le_bytes().repeat(2)[..16]);
        assert_eq!(key.len(), 24); // 16 bytes -> 24 base64 chars
        assert!(key.ends_with('='));
    }
}
