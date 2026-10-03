//! Optional HTTP transport, off by default.
//!
//! Stdio is the primary transport and needs no authentication: the client *is*
//! the parent process, and the OS enforces that. A listening socket has none of
//! that for free, so everything here exists to put it back:
//!
//! * **Loopback only.** Binding anywhere else is refused outright, not warned
//!   about. This is plaintext HTTP; on a LAN the bearer token would cross the
//!   wire in the clear on every request.
//! * **Bearer token, always.** There is no anonymous mode. Comparison is
//!   constant-time so the token cannot be recovered a byte at a time.
//! * **`Origin` refused by default.** Any browser on the machine can reach
//!   `127.0.0.1` — this is the DNS-rebinding class of attack, and it is why a
//!   local port is not the same thing as a private one. A real MCP client sends
//!   no `Origin` header at all, so refusing every unlisted one costs nothing.
//! * **Bounded everything.** Header block, body, read time and the number of
//!   connections served at once all have caps; a half-open connection cannot
//!   hold resources indefinitely.
//! * **Concurrent, and checked per request.** Connections are served in
//!   parallel so a `notifications/cancelled` POST can reach a `tools/call`
//!   POST that is still running. Every connection runs the full origin, token
//!   and size checks itself; nothing carries over from another connection.
//!
//! The wire format is the JSON subset of MCP's Streamable HTTP transport: POST
//! a single JSON-RPC message, get a single JSON-RPC message back (or `202` for
//! a notification). Server-Sent Events are **not** implemented, so there is no
//! server-initiated streaming — `GET` on the endpoint is refused rather than
//! silently hanging.
//!
//! **No server-initiated frames.** One request, one response: there is nowhere
//! to put a `notifications/progress` message. `_meta.progressToken` is accepted
//! and ignored rather than refused, so a client written for stdio works here
//! too — it simply hears nothing until the call returns. Carrying them would
//! mean implementing SSE, which this transport deliberately does not.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::server::Server;

/// Largest header block accepted before the request is refused.
const MAX_HEADER_BYTES: usize = 16 * 1024;

/// Settings for the HTTP transport.
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// Address to listen on. Must be loopback; [`HttpTransport::bind`] refuses
    /// anything else.
    pub bind: SocketAddr,
    /// Required bearer token. Generate one with [`generate_token`].
    pub token: String,
    /// Browser origins permitted to call the endpoint. Empty — the default —
    /// means any request carrying an `Origin` header is refused.
    pub allowed_origins: Vec<String>,
    /// Largest request body accepted.
    pub max_body_bytes: usize,
    /// How long one request may take to arrive. Bounds reading only; a tool
    /// call that has been accepted runs for as long as it needs.
    pub read_timeout: Duration,
    /// Most connections served at once. Connections are served concurrently so
    /// a `notifications/cancelled` can arrive while another request's call is
    /// still running; past this bound a new connection is refused with 503
    /// rather than queued.
    pub max_connections: usize,
}

impl Default for HttpConfig {
    fn default() -> Self {
        HttpConfig {
            bind: SocketAddr::from(([127, 0, 0, 1], 0)),
            token: String::new(),
            allowed_origins: Vec::new(),
            max_body_bytes: 8 * 1024 * 1024,
            read_timeout: Duration::from_secs(30),
            max_connections: 64,
        }
    }
}

/// A 256-bit token in hex, from the OS CSPRNG.
///
/// Reads `/dev/urandom` directly rather than pulling in a random-number crate:
/// this is the kernel's own entropy source, which is the thing such a crate
/// would call anyway.
pub fn generate_token() -> io::Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Compare two secrets without leaking where they first differ.
///
/// Length is compared up front: it is not secret in any threat model that
/// matters here, and a fixed-length token makes it constant anyway.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// A bound listener, ready to serve.
#[derive(Debug)]
pub struct HttpTransport {
    listener: TcpListener,
    cfg: HttpConfig,
}

impl HttpTransport {
    /// Bind the listener, refusing an unsafe configuration rather than starting
    /// and hoping. Failing here is loud; failing later is a silent exposure.
    pub async fn bind(cfg: HttpConfig) -> io::Result<Self> {
        if !cfg.bind.ip().is_loopback() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "refusing to bind {} — the HTTP transport is loopback-only, because it \
                     speaks plaintext and would put the bearer token on the wire",
                    cfg.bind
                ),
            ));
        }
        if cfg.max_connections == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "max_connections must be at least 1",
            ));
        }
        if cfg.token.len() < 16 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "refusing to start — the HTTP transport requires a bearer token of at least \
                 16 characters; there is no anonymous mode",
            ));
        }
        let listener = TcpListener::bind(cfg.bind).await?;
        Ok(HttpTransport { listener, cfg })
    }

    /// The address actually bound, which differs from the requested one when
    /// port 0 was asked for.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Accept connections until the listener errors fatally.
    ///
    /// One request per connection: `Connection: close` keeps the state machine
    /// small, and a local control plane has no throughput problem that
    /// keep-alive would solve.
    ///
    /// Each connection is its own task, up to `max_connections`, and each runs
    /// the full origin/auth/size checks itself: nothing is inherited from an
    /// earlier connection. Concurrency is what lets a `notifications/cancelled`
    /// POST reach a `tools/call` POST that is still running.
    pub async fn serve(self, server: Arc<Server>) -> io::Result<()> {
        let slots = Arc::new(tokio::sync::Semaphore::new(self.cfg.max_connections));
        let refusals = Arc::new(tokio::sync::Semaphore::new(MAX_REFUSALS));
        loop {
            let (stream, peer) = match self.listener.accept().await {
                Ok(x) => x,
                // A single connection failing to set up is not fatal.
                Err(e) if is_transient(&e) => continue,
                Err(e) => return Err(e),
            };
            // Checked before any byte is read, so an unauthenticated flood
            // costs a refused socket, not a served connection. The refusal is
            // itself bounded: past `MAX_REFUSALS` the socket is just dropped,
            // so a flood cannot turn refusals into unbounded tasks.
            let Ok(permit) = slots.clone().try_acquire_owned() else {
                tracing::warn!(%peer, "http connection refused: at max_connections");
                if let Ok(refusal) = refusals.clone().try_acquire_owned() {
                    tokio::spawn(async move {
                        let _refusal = refusal;
                        refuse(stream).await;
                    });
                }
                continue;
            };
            let server = server.clone();
            let cfg = self.cfg.clone();
            tokio::spawn(async move {
                let _permit = permit;
                if let Err(e) = handle_connection(stream, &server, &cfg).await {
                    tracing::debug!(%peer, error = %e, "http connection ended");
                }
            });
        }
    }
}

/// Most 503 refusals being written at once.
const MAX_REFUSALS: usize = 16;

/// Tell a client the server is full, then drain so the 503 is not lost to an
/// RST. Bounded by the same limits as any other refusal.
async fn refuse(mut stream: TcpStream) {
    let text = status_response(503, "too many concurrent connections");
    let ok = tokio::time::timeout(DRAIN_IDLE, stream.write_all(text.as_bytes())).await;
    if matches!(ok, Ok(Ok(()))) {
        drain(&mut stream, Drain::UntilIdle).await;
    }
    let _ = stream.shutdown().await;
}

fn is_transient(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::Interrupted
    )
}

/// What the connection still owes the client before it can close cleanly.
///
/// Closing a socket that still has unread bytes in its receive queue sends an
/// RST rather than a FIN, and an RST discards data already in flight — which
/// means the client never sees the very error we just wrote. Refusing a large
/// request is exactly when this happens, so a refusal has to drain first.
enum Drain {
    /// Nothing outstanding.
    None,
    /// This many declared body bytes were never read.
    Bytes(usize),
    /// Length unknown; read until the client stops or the deadline passes.
    UntilIdle,
}

/// A response plus the cleanup it implies.
struct Reply {
    text: String,
    drain: Drain,
}

impl Reply {
    fn done(text: String) -> Self {
        Reply {
            text,
            drain: Drain::None,
        }
    }
}

/// Upper bound on bytes discarded while draining, and how long to wait when the
/// outstanding length is unknown. Both exist so a client that keeps talking
/// cannot hold a connection — or a task — open indefinitely.
const MAX_DRAIN_BYTES: usize = 1024 * 1024;
const DRAIN_IDLE: Duration = Duration::from_millis(250);

/// A parsed request line plus the headers we care about.
struct Parsed {
    method: String,
    path: String,
    content_length: Option<usize>,
    authorization: Option<String>,
    origin: Option<String>,
    chunked: bool,
}

async fn handle_connection(
    mut stream: TcpStream,
    server: &Server,
    cfg: &HttpConfig,
) -> io::Result<()> {
    // The timeout covers *receiving* the request only. A tool call may
    // legitimately run longer, and dropping it at the read deadline would
    // abandon a half-finished action.
    let outcome = tokio::time::timeout(cfg.read_timeout, read_request(&mut stream, cfg)).await;
    let reply = match outcome {
        Ok(Ok(Request::Body(text))) => {
            // The same dispatch path stdio uses — the transport authenticates,
            // it does not get its own copy of the protocol or its own way past
            // the gate.
            match server.handle_line(&text).await {
                Some(response) => Reply::done(json_response(200, &response)),
                // A notification has no reply, and neither does a request the
                // client cancelled. 202 says "accepted, nothing to return".
                None => Reply::done(status_response(202, "accepted")),
            }
        }
        Ok(Ok(Request::Refused(reply))) => reply,
        Ok(Err(e)) => return Err(e),
        Err(_) => Reply::done(status_response(408, "request timed out")),
    };
    stream.write_all(reply.text.as_bytes()).await?;
    stream.flush().await?;
    // Drain before closing, or the refusal above is lost to an RST. Failures
    // here are not interesting: the response is already on the wire.
    drain(&mut stream, reply.drain).await;
    let _ = stream.shutdown().await;
    Ok(())
}

/// Discard whatever the client is still sending, bounded in both bytes and time.
async fn drain(stream: &mut TcpStream, what: Drain) {
    let budget = match what {
        Drain::None => return,
        Drain::Bytes(n) => n.min(MAX_DRAIN_BYTES),
        Drain::UntilIdle => MAX_DRAIN_BYTES,
    };
    let mut sink = vec![0u8; 16 * 1024];
    let mut left = budget;
    while left > 0 {
        let take = sink.len().min(left);
        match tokio::time::timeout(DRAIN_IDLE, stream.read(&mut sink[..take])).await {
            // EOF, error, or the client went quiet: nothing more is coming.
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => return,
            Ok(Ok(n)) => left -= n,
        }
    }
}

/// What reading one request produced.
enum Request {
    /// A request that passed every check; its body is ready to dispatch.
    Body(String),
    /// Refused before dispatch; the reply says why.
    Refused(Reply),
}

/// Read one request and run every check on it, without dispatching.
async fn read_request(stream: &mut TcpStream, cfg: &HttpConfig) -> io::Result<Request> {
    read_request_inner(stream, cfg).await.map(|r| match r {
        Ok(text) => Request::Body(text),
        Err(reply) => Request::Refused(reply),
    })
}

async fn read_request_inner(
    stream: &mut TcpStream,
    cfg: &HttpConfig,
) -> io::Result<Result<String, Reply>> {
    // ---- headers -----------------------------------------------------------
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let head_end = loop {
        if let Some(at) = find_header_end(&buf) {
            break at;
        }
        if buf.len() > MAX_HEADER_BYTES {
            return Ok(Err(Reply {
                text: status_response(431, "header block too large"),
                drain: Drain::UntilIdle,
            }));
        }
        let mut chunk = [0u8; 1024];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            // Client hung up before finishing the header block.
            return Ok(Err(Reply::done(status_response(400, "incomplete request"))));
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let Some(parsed) = parse_head(&buf[..head_end]) else {
        return Ok(Err(Reply::done(status_response(400, "malformed request"))));
    };

    // ---- checks, cheapest and most conclusive first -------------------------
    if parsed.chunked {
        // Chunked framing buys nothing for single-message JSON-RPC and is a
        // classic request-smuggling surface. Refuse rather than implement.
        return Ok(Err(Reply {
            text: status_response(
                411,
                "chunked encoding is not supported; send Content-Length",
            ),
            drain: Drain::UntilIdle,
        }));
    }
    if parsed.method != "POST" {
        // GET would be the SSE stream in the full spec; we do not implement it,
        // so say so instead of leaving the client waiting on a stream.
        return Ok(Err(Reply {
            text: status_response(
                405,
                "only POST is supported (this transport does not implement SSE)",
            ),
            drain: unread_body(&parsed, &buf, head_end),
        }));
    }
    if parsed.path != "/" && parsed.path != "/mcp" {
        return Ok(Err(Reply {
            text: status_response(404, "not found"),
            drain: unread_body(&parsed, &buf, head_end),
        }));
    }
    // Origin before auth: a browser-mounted request should be refused on the
    // grounds that it is a browser, not told whether its token guess was right.
    if let Some(origin) = &parsed.origin {
        if !cfg.allowed_origins.iter().any(|a| a == origin) {
            return Ok(Err(Reply {
                text: status_response(
                    403,
                    "origin not allowed; a browser must not drive this endpoint",
                ),
                drain: unread_body(&parsed, &buf, head_end),
            }));
        }
    }
    let presented = parsed
        .authorization
        .as_deref()
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if !constant_time_eq(presented.as_bytes(), cfg.token.as_bytes()) {
        return Ok(Err(Reply {
            text: unauthorized(),
            drain: unread_body(&parsed, &buf, head_end),
        }));
    }
    let Some(len) = parsed.content_length else {
        return Ok(Err(Reply {
            text: status_response(411, "Content-Length required"),
            drain: Drain::UntilIdle,
        }));
    };
    if len > cfg.max_body_bytes {
        return Ok(Err(Reply {
            text: status_response(413, "request body too large"),
            drain: unread_body(&parsed, &buf, head_end),
        }));
    }

    // ---- body --------------------------------------------------------------
    let mut body = buf[head_end..].to_vec();
    body.truncate(len.min(body.len()));
    while body.len() < len {
        let mut chunk = vec![0u8; (len - body.len()).min(64 * 1024)];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(Err(Reply::done(status_response(
                400,
                "request body shorter than Content-Length",
            ))));
        }
        body.extend_from_slice(&chunk[..n]);
    }

    Ok(Ok(String::from_utf8_lossy(&body).into_owned()))
}

/// How much of the declared body has not been read yet.
fn unread_body(parsed: &Parsed, buf: &[u8], head_end: usize) -> Drain {
    match parsed.content_length {
        Some(len) => Drain::Bytes(len.saturating_sub(buf.len() - head_end)),
        None => Drain::None,
    }
}

/// Find the end of the header block, tolerating bare-LF line endings.
fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .or_else(|| buf.windows(2).position(|w| w == b"\n\n").map(|i| i + 2))
}

fn parse_head(head: &[u8]) -> Option<Parsed> {
    let text = std::str::from_utf8(head).ok()?;
    let mut lines = text.split(['\r', '\n']).filter(|l| !l.is_empty());
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?;
    let version = parts.next().unwrap_or("HTTP/1.1");
    if !version.starts_with("HTTP/") {
        return None;
    }
    // Strip any query string; this endpoint takes no parameters.
    let path = target.split(['?', '#']).next().unwrap_or("/").to_string();

    let mut parsed = Parsed {
        method,
        path,
        content_length: None,
        authorization: None,
        origin: None,
        chunked: false,
    };
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            // A duplicated Content-Length is a smuggling primitive; refuse the
            // whole request rather than picking one.
            "content-length" => {
                if parsed.content_length.is_some() {
                    return None;
                }
                parsed.content_length = Some(value.parse().ok()?);
            }
            "transfer-encoding" => parsed.chunked = true,
            "authorization" => parsed.authorization = Some(value.to_string()),
            "origin" => parsed.origin = Some(value.to_string()),
            _ => {}
        }
    }
    Some(parsed)
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

fn json_response(code: u16, body: &str) -> String {
    format!(
        "HTTP/1.1 {code} {}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        reason(code),
        body.len()
    )
}

/// Errors are returned as JSON so a client parsing one shape does not have to
/// special-case another. The message is deliberately terse.
fn status_response(code: u16, message: &str) -> String {
    let body = serde_json::json!({ "error": message }).to_string();
    json_response(code, &body)
}

fn unauthorized() -> String {
    let body = serde_json::json!({ "error": "invalid or missing bearer token" }).to_string();
    format!(
        "HTTP/1.1 401 Unauthorized\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         WWW-Authenticate: Bearer\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_still_compares_correctly() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn generated_tokens_are_long_and_distinct() {
        let a = generate_token().unwrap();
        let b = generate_token().unwrap();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "tokens must not repeat");
    }

    #[test]
    fn header_end_handles_both_line_endings() {
        assert_eq!(find_header_end(b"GET / HTTP/1.1\r\n\r\nbody"), Some(18));
        assert_eq!(find_header_end(b"GET / HTTP/1.1\n\nbody"), Some(16));
        assert_eq!(find_header_end(b"GET / HTTP/1.1\r\n"), None);
    }

    #[test]
    fn duplicate_content_length_is_refused() {
        let head = b"POST / HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 9\r\n\r\n";
        assert!(parse_head(head).is_none());
    }

    #[test]
    fn query_strings_are_stripped_from_the_path() {
        let head = b"POST /mcp?token=leak HTTP/1.1\r\nContent-Length: 0\r\n\r\n";
        assert_eq!(parse_head(head).unwrap().path, "/mcp");
    }
}
