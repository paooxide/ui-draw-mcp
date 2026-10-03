//! Reproducible latency benchmark for `agentctl serve`.
//!
//! Launches the real server binary as a child process, speaks MCP over its
//! stdio, and times tool calls. Every number in the output comes from a call
//! made by this run; an operation that fails is recorded as skipped with the
//! server's error code, never replaced by a constant.
//!
//! Run: `cargo run --release --example bench -- --n 30`
//! See docs/bench/README.md for what each timed region contains.
//!
//! This is an example, not a test: `cargo test` never runs it.

use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// Row order in the Markdown summary. A new operation not listed here sorts last.
const OP_ORDER: &[&str] = &[
    "ping",
    "list_windows",
    "capture_screen",
    "ocr_region",
    "browser_snapshot",
    "browser_act",
    "mouse_action_move",
];

const FIXTURE: &str = "<!doctype html><html><body><h1>agentctl bench fixture</h1>\
<p>A fixed page served by the bench.</p>\
<input id='name' placeholder='your name'>\
<button id='go' onclick=\"document.getElementById('out').textContent=String(++window.n||(window.n=1))\">Go</button>\
<p id='out'></p></body></html>";

// ---------------------------------------------------------------- arguments

struct Args {
    n: usize,
    warmup: usize,
    out_dir: PathBuf,
    bin: Option<PathBuf>,
}

fn parse_args() -> Args {
    let mut a = Args {
        n: 30,
        warmup: 5,
        out_dir: repo_root().join("docs/bench/results"),
        bin: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut val = |name: &str| {
            it.next()
                .unwrap_or_else(|| die(&format!("{name} needs a value")))
        };
        match flag.as_str() {
            "--n" => {
                a.n = val("--n")
                    .parse()
                    .unwrap_or_else(|_| die("--n must be an integer"))
            }
            "--warmup" => {
                a.warmup = val("--warmup")
                    .parse()
                    .unwrap_or_else(|_| die("--warmup must be an integer"))
            }
            "--out-dir" => a.out_dir = PathBuf::from(val("--out-dir")),
            "--bin" => a.bin = Some(PathBuf::from(val("--bin"))),
            "-h" | "--help" => {
                println!(
                    "usage: bench [--n 30] [--warmup 5] [--out-dir DIR] [--bin PATH-TO-agentctl]\n\
                     Set AGENTCTL_LIVE_GUI=1 to include the pointer-moving operation."
                );
                std::process::exit(0);
            }
            other => die(&format!("unknown flag {other}")),
        }
    }
    if a.n < 2 {
        die("--n must be at least 2 (standard deviation needs two samples)");
    }
    a
}

fn die(msg: &str) -> ! {
    eprintln!("bench: {msg}");
    std::process::exit(2);
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("agentctl has a parent directory")
        .to_path_buf()
}

/// The server binary. Unless `--bin` names one, build it first with the same
/// profile as this example: `cargo run --example` does not build the package's
/// binary, and a stale one would silently be what gets measured. The build is
/// a no-op when it is current.
fn find_bin(explicit: &Option<PathBuf>) -> PathBuf {
    if let Some(p) = explicit {
        return p.clone();
    }
    let mut build = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    build.args(["build", "-p", "agentctl", "--bin", "agentctl"]);
    if !cfg!(debug_assertions) {
        build.arg("--release");
    }
    match build.status() {
        Ok(s) if s.success() => {}
        _ => die("`cargo build -p agentctl --bin agentctl` failed"),
    }
    let exe = std::env::current_exe().expect("current exe");
    exe.parent()
        .and_then(Path::parent)
        .map(|d| d.join(format!("agentctl{}", std::env::consts::EXE_SUFFIX)))
        .filter(|c| c.exists())
        .unwrap_or_else(|| die("built agentctl not found next to the example; pass --bin"))
}

// ------------------------------------------------------------------ session

/// One call as seen by the client.
struct Outcome {
    ms: f64,
    bytes: usize,
    ok: bool,
    /// Server error code (`PERM_DENIED`, ...) or `rpc:<code>` for a protocol error.
    error_code: Option<String>,
    error_message: Option<String>,
    data: Option<Value>,
}

struct Session {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: u64,
    audit_dir: PathBuf,
    /// Names of every `tools/call` made, in order, so audit post records can be
    /// matched back to calls. `ping` is a protocol method and is not audited.
    audited_calls: Vec<String>,
    _tmp: tempfile::TempDir,
}

impl Session {
    fn start(bin: &Path, config_body: &str) -> Result<Session, String> {
        let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
        let audit_dir = tmp.path().join("audit");
        let config = format!(
            "[policy]\nmode = \"autonomous\"\naudit_dir = \"{}\"\nkill_switch_file = \"{}\"\n{config_body}",
            audit_dir.display(),
            tmp.path().join("STOP").display(),
        );
        let config_path = tmp.path().join("config.toml");
        fs::write(&config_path, config).map_err(|e| e.to_string())?;
        let stderr = fs::File::create(tmp.path().join("stderr.log")).map_err(|e| e.to_string())?;
        let mut child = Command::new(bin)
            .arg("serve")
            .env("AGENTCTL_CONFIG", &config_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", bin.display()))?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(l) = line else { break };
                if tx.send(l).is_err() {
                    break;
                }
            }
        });
        let mut s = Session {
            child,
            stdin,
            lines: rx,
            next_id: 1,
            audit_dir,
            audited_calls: Vec::new(),
            _tmp: tmp,
        };
        let init = s.rpc(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "agentctl-bench", "version": "1"}
            }),
        )?;
        if init.error_code.is_some() {
            return Err(format!("initialize failed: {:?}", init.error_message));
        }
        s.notify("notifications/initialized")?;
        Ok(s)
    }

    fn send_line(&mut self, v: &Value) -> Result<(), String> {
        let mut line = serde_json::to_string(v).map_err(|e| e.to_string())?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.flush())
            .map_err(|e| format!("write to server failed: {e}"))
    }

    fn notify(&mut self, method: &str) -> Result<(), String> {
        self.send_line(&json!({"jsonrpc": "2.0", "method": method}))
    }

    /// Send one JSON-RPC request and wait for its response. The clock covers
    /// the pipe write through the arrival of the response line, and stops
    /// before the response is parsed.
    fn rpc(&mut self, method: &str, params: Value) -> Result<Outcome, String> {
        let id = self.next_id;
        self.next_id += 1;
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let mut line = serde_json::to_string(&req).map_err(|e| e.to_string())?;
        line.push('\n');
        let start = Instant::now();
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.flush())
            .map_err(|e| format!("write to server failed: {e}"))?;
        loop {
            let raw = self
                .lines
                .recv_timeout(CALL_TIMEOUT)
                .map_err(|e| format!("no response to {method}: {e}"))?;
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            let v: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
            if v.get("id") != Some(&json!(id)) {
                continue; // progress notification or stale line
            }
            return Ok(interpret(&v, ms, raw.len()));
        }
    }

    fn call(&mut self, tool: &str, args: Value) -> Result<Outcome, String> {
        self.audited_calls.push(tool.to_string());
        self.rpc("tools/call", json!({"name": tool, "arguments": args}))
    }

    /// Close the session and return the audit post records (tool, latency_ms)
    /// in the order they were written.
    fn finish(mut self) -> Result<Vec<(String, Option<u64>)>, String> {
        drop(self.stdin);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
                _ => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
            }
        }
        let mut file = None;
        for e in fs::read_dir(&self.audit_dir).map_err(|e| format!("audit dir: {e}"))? {
            let p = e.map_err(|e| e.to_string())?.path();
            if p.extension().is_some_and(|x| x == "jsonl") {
                file = Some(p);
            }
        }
        let path = file.ok_or("no audit .jsonl was written")?;
        let mut text = String::new();
        fs::File::open(path)
            .and_then(|mut f| f.read_to_string(&mut text))
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for l in text.lines().filter(|l| !l.trim().is_empty()) {
            let v: Value = serde_json::from_str(l).map_err(|e| format!("audit line: {e}"))?;
            if v["phase"] == "post" {
                out.push((
                    v["tool"].as_str().unwrap_or("").to_string(),
                    v["latency_ms"].as_u64(),
                ));
            }
        }
        Ok(out)
    }
}

fn interpret(v: &Value, ms: f64, bytes: usize) -> Outcome {
    if let Some(err) = v.get("error") {
        return Outcome {
            ms,
            bytes,
            ok: false,
            error_code: Some(format!("rpc:{}", err["code"])),
            error_message: err["message"].as_str().map(str::to_string),
            data: None,
        };
    }
    let result = &v["result"];
    // Tool results carry an envelope as JSON text in the first content block.
    if let Some(text) = result["content"][0]["text"].as_str() {
        if let Ok(env) = serde_json::from_str::<Value>(text) {
            if env.get("ok").is_some() {
                let ok = env["ok"] == true;
                return Outcome {
                    ms,
                    bytes,
                    ok,
                    error_code: (!ok).then(|| {
                        env["error"]["code"]
                            .as_str()
                            .unwrap_or("UNKNOWN")
                            .to_string()
                    }),
                    error_message: env["error"]["message"].as_str().map(str::to_string),
                    data: env.get("data").cloned(),
                };
            }
        }
    }
    Outcome {
        ms,
        bytes,
        ok: true,
        error_code: None,
        error_message: None,
        data: Some(result.clone()),
    }
}

// --------------------------------------------------------------- statistics

fn stats(samples: &[f64]) -> Value {
    let mut s = samples.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    let n = s.len();
    let mean = s.iter().sum::<f64>() / n as f64;
    let var = s.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n as f64 - 1.0);
    let median = if n % 2 == 1 {
        s[n / 2]
    } else {
        (s[n / 2 - 1] + s[n / 2]) / 2.0
    };
    // Nearest-rank percentile: with small n this is simply one of the largest samples.
    let p95 = s[((0.95 * n as f64).ceil() as usize).clamp(1, n) - 1];
    let r = |x: f64| (x * 1000.0).round() / 1000.0;
    json!({
        "n": n,
        "min": r(s[0]),
        "median": r(median),
        "p95": r(p95),
        "max": r(s[n - 1]),
        "stddev": r(var.sqrt()),
    })
}

// -------------------------------------------------------------- operations

/// What to send for an operation.
enum Call {
    Ping,
    Tool(&'static str, Value),
}

struct Measured {
    client_ms: Vec<f64>,
    bytes: Vec<f64>,
    /// Index into `audited_calls` of each *sampled* (not warm-up) call.
    audit_idx: Vec<usize>,
}

/// Run warm-up then sample calls. `Err` is the reason the operation is skipped.
fn measure(s: &mut Session, call: &Call, warmup: usize, n: usize) -> Result<Measured, String> {
    let mut m = Measured {
        client_ms: Vec::new(),
        bytes: Vec::new(),
        audit_idx: Vec::new(),
    };
    for i in 0..warmup + n {
        let idx = s.audited_calls.len();
        let out = match call {
            Call::Ping => s.rpc("ping", json!({}))?,
            Call::Tool(name, args) => s.call(name, args.clone())?,
        };
        if !out.ok {
            return Err(format!(
                "{}|{}",
                out.error_code.unwrap_or_default(),
                out.error_message.unwrap_or_default()
            ));
        }
        if i >= warmup {
            m.client_ms.push(out.ms);
            m.bytes.push(out.bytes as f64);
            m.audit_idx.push(idx);
        }
    }
    Ok(m)
}

fn skipped(reason: &str) -> Value {
    match reason.split_once('|') {
        Some((code, msg)) if !code.is_empty() => json!({"skipped": msg, "error_code": code}),
        Some((_, msg)) => json!({"skipped": msg}),
        None => json!({"skipped": reason}),
    }
}

/// Summarise one measured operation, joining server-side latency from the audit
/// records when the session's audit log lines up with the calls made.
fn summarise(
    tool: &str,
    m: &Measured,
    audit: &Result<Vec<(String, Option<u64>)>, String>,
    audited: &[String],
    audit_expected: bool,
) -> Value {
    let mut v = json!({
        "tool": tool,
        "samples_client_ms": m.client_ms.iter().map(|x| (x * 1000.0).round() / 1000.0).collect::<Vec<_>>(),
        "client_ms": stats(&m.client_ms),
        "response_bytes_median": stats(&m.bytes)["median"],
    });
    if !audit_expected {
        v["server_ms"] = Value::Null;
        v["server_note"] = json!("protocol-level method; the server does not audit it");
        return v;
    }
    match audit {
        Err(e) => {
            v["server_ms"] = Value::Null;
            v["server_note"] = json!(format!("audit log unreadable: {e}"));
        }
        Ok(posts) if posts.len() != audited.len() => {
            v["server_ms"] = Value::Null;
            v["server_note"] = json!(format!(
                "audit has {} post records for {} calls; not joined",
                posts.len(),
                audited.len()
            ));
        }
        Ok(posts) => {
            let mut lat = Vec::new();
            for &i in &m.audit_idx {
                match (&posts[i], audited.get(i)) {
                    ((t, Some(l)), Some(name)) if t == name => lat.push(*l as f64),
                    _ => {
                        v["server_ms"] = Value::Null;
                        v["server_note"] =
                            json!("audit record missing latency_ms or out of order; not joined");
                        return v;
                    }
                }
            }
            v["samples_server_ms"] = json!(lat);
            v["server_ms"] = stats(&lat);
            v["server_note"] =
                json!("latency_ms is an integer in the audit log: sub-millisecond work reads 0");
        }
    }
    v
}

// ------------------------------------------------------------------ fixture

fn serve_fixture() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
    let port = listener.local_addr().expect("addr").port();
    thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            thread::spawn(move || handle(conn));
        }
    });
    port
}

fn handle(mut conn: TcpStream) {
    let mut buf = [0u8; 4096];
    let _ = conn.read(&mut buf);
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{FIXTURE}",
        FIXTURE.len()
    );
    let _ = conn.write_all(resp.as_bytes());
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

// -------------------------------------------------------------- environment

fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

fn chrome_binary() -> Option<&'static str> {
    // Same list the server searches.
    const BINS: &[&str] = &[
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/snap/bin/chromium",
    ];
    BINS.iter().copied().find(|p| Path::new(p).exists())
}

fn environment() -> Value {
    let os = std::env::consts::OS;
    let (os_version, cpu) = match os {
        "macos" => (
            run("sw_vers", &["-productVersion"]),
            run("sysctl", &["-n", "machdep.cpu.brand_string"]),
        ),
        "linux" => {
            let rel = fs::read_to_string("/etc/os-release").unwrap_or_default();
            let pretty = rel
                .lines()
                .find_map(|l| l.strip_prefix("PRETTY_NAME="))
                .map(|s| s.trim_matches('"').to_string());
            let info = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
            let cpu = info
                .lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split_once(':'))
                .map(|(_, v)| v.trim().to_string());
            (pretty, cpu)
        }
        _ => (None, None),
    };
    let resolution = (os == "macos")
        .then(|| run("system_profiler", &["SPDisplaysDataType"]))
        .flatten()
        .map(|t| {
            t.lines()
                .filter(|l| l.trim_start().starts_with("Resolution:"))
                .map(|l| l.trim().to_string())
                .collect::<Vec<_>>()
        });
    let chrome = chrome_binary().and_then(|b| run(b, &["--version"]));
    let root = repo_root();
    let git = |args: &[&str]| {
        let mut full = vec!["-C", root.to_str().expect("utf-8 path")];
        full.extend_from_slice(args);
        run("git", &full)
    };
    json!({
        "os": os,
        "os_version": os_version,
        "arch": std::env::consts::ARCH,
        "cpu": cpu,
        "display_resolution": resolution,
        "chrome_version": chrome,
        "git_commit": git(&["rev-parse", "HEAD"]),
        "git_commit_short": git(&["rev-parse", "--short", "HEAD"]),
        // Tracked files only: the results file this run writes is untracked.
        "git_dirty": git(&["status", "--porcelain", "--untracked-files=no"]).is_some(),
        "rustc": run("rustc", &["--version"]),
        "bench_build": if cfg!(debug_assertions) { "debug" } else { "release" },
    })
}

// --------------------------------------------------------------------- main

fn main() {
    let args = parse_args();
    let bin = find_bin(&args.bin);
    let live_gui = std::env::var("AGENTCTL_LIVE_GUI").is_ok_and(|v| v == "1");
    if cfg!(debug_assertions) {
        eprintln!("bench: warning: built without --release; the bench itself is unoptimised");
    }
    eprintln!(
        "bench: server {}, n={}, warmup={}",
        bin.display(),
        args.n,
        args.warmup
    );

    let mut env = environment();
    let mut ops = serde_json::Map::new();

    // ---- native session: ping, list_windows, capture_screen, ocr_region
    let native_cfg = "categories = [\"vision\", \"window\"]\n";
    match Session::start(&bin, native_cfg) {
        Err(e) => {
            for name in ["ping", "list_windows", "capture_screen", "ocr_region"] {
                ops.insert(
                    name.into(),
                    json!({"skipped": format!("server did not start: {e}")}),
                );
            }
        }
        Ok(mut s) => {
            // Setup, not timed into any operation: display geometry for the report.
            if let Ok(o) = s.call("list_displays", json!({})) {
                env["displays"] = if o.ok {
                    o.data.unwrap_or(Value::Null)
                } else {
                    json!({"skipped": o.error_message, "error_code": o.error_code})
                };
            }
            let plan: Vec<(&str, Call)> = vec![
                ("ping", Call::Ping),
                ("list_windows", Call::Tool("list_windows", json!({}))),
                // force: re-send even when the frame matches the last one, so
                // every sample pays for a full capture rather than the dedup path.
                (
                    "capture_screen",
                    Call::Tool("capture_screen", json!({"force": true})),
                ),
                (
                    "ocr_region",
                    Call::Tool(
                        "ocr_region",
                        json!({"region": {"x": 0, "y": 0, "w": 400, "h": 100}}),
                    ),
                ),
            ];
            let mut measured = Vec::new();
            for (name, call) in &plan {
                eprintln!("bench: {name}");
                measured.push((*name, measure(&mut s, call, args.warmup, args.n)));
            }
            let audited = s.audited_calls.clone();
            let audit = s.finish();
            for (name, res) in measured {
                let v = match res {
                    Ok(m) => summarise(name, &m, &audit, &audited, name != "ping"),
                    Err(e) => skipped(&e),
                };
                ops.insert(name.into(), v);
            }
        }
    }

    // ---- browser session: browser_snapshot, browser_act on a local fixture
    let fixture_port = serve_fixture();
    let browser_cfg = "categories = [\"browser\"]\n[browser]\nallow_private = true\n";
    let browser_ops = ["browser_snapshot", "browser_act"];
    match browser_session(&bin, browser_cfg, fixture_port, &args) {
        Ok(map) => ops.extend(map),
        Err(e) => {
            for name in browser_ops {
                ops.insert(name.into(), skipped(&e));
            }
        }
    }

    // ---- GUI session: pointer movement, only on explicit request
    if live_gui {
        match gui_session(&bin, &args) {
            Ok(v) => {
                ops.insert("mouse_action_move".into(), v);
            }
            Err(e) => {
                ops.insert("mouse_action_move".into(), skipped(&e));
            }
        }
    } else {
        ops.insert(
            "mouse_action_move".into(),
            json!({"skipped": "moves the real cursor; set AGENTCTL_LIVE_GUI=1 to include it"}),
        );
    }

    let date = utc_date();
    let short = env["git_commit_short"]
        .as_str()
        .unwrap_or("nogit")
        .to_string();
    let os = std::env::consts::OS;
    let report = json!({
        "schema": 1,
        "date_utc": date,
        "n": args.n,
        "warmup": args.warmup,
        "environment": env,
        "operations": Value::Object(ops),
    });
    fs::create_dir_all(&args.out_dir).expect("create output dir");
    let stem = format!("{date}-{os}-{short}");
    let json_path = args.out_dir.join(format!("{stem}.json"));
    let md_path = args.out_dir.join(format!("{stem}.md"));
    fs::write(
        &json_path,
        serde_json::to_string_pretty(&report).expect("json") + "\n",
    )
    .expect("write json");
    let md = markdown(&report);
    fs::write(&md_path, &md).expect("write markdown");
    println!("{md}");
    eprintln!(
        "bench: wrote {} and {}",
        json_path.display(),
        md_path.display()
    );
}

fn browser_session(
    bin: &Path,
    cfg: &str,
    fixture_port: u16,
    args: &Args,
) -> Result<serde_json::Map<String, Value>, String> {
    let mut s = Session::start(bin, cfg)?;
    let cdp_port = free_port();
    let conn = s.call(
        "browser_connect",
        json!({"launch": {"headless": true, "port": cdp_port}}),
    )?;
    if !conn.ok {
        let code = conn.error_code.unwrap_or_default();
        let msg = conn.error_message.unwrap_or_default();
        let _ = s.finish();
        return Err(format!("{code}|browser_connect: {msg}"));
    }
    let browser_id = conn
        .data
        .as_ref()
        .and_then(|d| d["browser_id"].as_u64())
        .ok_or("browser_connect returned no browser_id")?;
    let setup = |s: &mut Session, tool: &str, a: Value| -> Result<Value, String> {
        let o = s.call(tool, a)?;
        if o.ok {
            Ok(o.data.unwrap_or(Value::Null))
        } else {
            Err(format!(
                "{}|{tool}: {}",
                o.error_code.unwrap_or_default(),
                o.error_message.unwrap_or_default()
            ))
        }
    };
    let result = (|| {
        let tabs = setup(
            &mut s,
            "browser_tabs",
            json!({"browser_id": browser_id, "action": "list"}),
        )?;
        let target = tabs["tabs"][0]["target_id"]
            .as_str()
            .ok_or("no tab to drive")?
            .to_string();
        setup(
            &mut s,
            "browser_navigate",
            json!({"target_id": target, "action": "goto", "url": format!("http://127.0.0.1:{fixture_port}/")}),
        )?;
        let plan = [
            (
                "browser_snapshot",
                Call::Tool(
                    "browser_snapshot",
                    json!({"target_id": target, "mode": "dom"}),
                ),
            ),
            (
                "browser_act",
                Call::Tool(
                    "browser_act",
                    json!({"target_id": target, "query": "#go", "by": "css", "action": "click"}),
                ),
            ),
        ];
        let mut measured = Vec::new();
        for (name, call) in &plan {
            eprintln!("bench: {name}");
            measured.push((*name, measure(&mut s, call, args.warmup, args.n)));
        }
        Ok::<_, String>(measured)
    })();
    // Always tear the browser down, whatever happened above.
    let _ = s.call(
        "browser_disconnect",
        json!({"browser_id": browser_id, "kill": true}),
    );
    let audited = s.audited_calls.clone();
    let audit = s.finish();
    let measured = result?;
    let mut out = serde_json::Map::new();
    for (name, res) in measured {
        let v = match res {
            Ok(m) => summarise(name, &m, &audit, &audited, true),
            Err(e) => skipped(&e),
        };
        out.insert(name.into(), v);
    }
    Ok(out)
}

fn gui_session(bin: &Path, args: &Args) -> Result<Value, String> {
    let mut s = Session::start(bin, "categories = [\"input\"]\n")?;
    // Two nearby fixed points, alternated, so every sample is a real move.
    let mut flip = false;
    let mut m = Measured {
        client_ms: Vec::new(),
        bytes: Vec::new(),
        audit_idx: Vec::new(),
    };
    let mut failure = None;
    for i in 0..args.warmup + args.n {
        flip = !flip;
        let x = if flip { 300 } else { 340 };
        let idx = s.audited_calls.len();
        let o = s.call("mouse_action", json!({"type": "move", "x": x, "y": 300}))?;
        if !o.ok {
            failure = Some(format!(
                "{}|{}",
                o.error_code.unwrap_or_default(),
                o.error_message.unwrap_or_default()
            ));
            break;
        }
        if i >= args.warmup {
            m.client_ms.push(o.ms);
            m.bytes.push(o.bytes as f64);
            m.audit_idx.push(idx);
        }
    }
    let audited = s.audited_calls.clone();
    let audit = s.finish();
    match failure {
        Some(f) => Err(f),
        None => Ok(summarise("mouse_action", &m, &audit, &audited, true)),
    }
}

// ------------------------------------------------------------------- output

/// Today's UTC date, from the system clock (civil-from-days).
fn utc_date() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs() as i64;
    let z = secs / 86_400 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

fn fmt(v: &Value, key: &str) -> String {
    v[key].as_f64().map_or("-".into(), |x| format!("{x:.2}"))
}

fn markdown(r: &Value) -> String {
    let e = &r["environment"];
    let s = |k: &str| e[k].as_str().unwrap_or("unknown").to_string();
    let mut md = format!(
        "# agentctl latency bench, {} ({} {})\n\n\
         - Commit: `{}`{}\n- OS: {} {} ({})\n- CPU: {}\n- Display: {}\n- Chrome: {}\n- Samples: {} per operation after {} warm-up calls\n\n",
        r["date_utc"].as_str().unwrap_or(""),
        s("os"),
        s("arch"),
        s("git_commit_short"),
        if e["git_dirty"] == true { " (tracked files modified)" } else { "" },
        s("os"),
        s("os_version"),
        s("arch"),
        s("cpu"),
        e["display_resolution"],
        s("chrome_version"),
        r["n"],
        r["warmup"],
    );
    md.push_str("Milliseconds. `client` is the full round trip over the stdio pipe; `server` is the integer `latency_ms` from the audit log.\n\n");
    md.push_str("| operation | which | min | median | p95 | max | stddev |\n|---|---|---:|---:|---:|---:|---:|\n");
    let mut skips = Vec::new();
    let all = r["operations"].as_object().expect("operations");
    let mut order: Vec<&String> = all.keys().collect();
    order.sort_by_key(|k| OP_ORDER.iter().position(|o| o == k).unwrap_or(usize::MAX));
    for name in order {
        let op = &all[name];
        if let Some(reason) = op["skipped"].as_str() {
            let code = op["error_code"]
                .as_str()
                .map(|c| format!(" [{c}]"))
                .unwrap_or_default();
            skips.push(format!("- `{name}`: {reason}{code}"));
            continue;
        }
        let c = &op["client_ms"];
        md.push_str(&format!(
            "| `{name}` | client | {} | {} | {} | {} | {} |\n",
            fmt(c, "min"),
            fmt(c, "median"),
            fmt(c, "p95"),
            fmt(c, "max"),
            fmt(c, "stddev")
        ));
        if op["server_ms"].is_object() {
            let c = &op["server_ms"];
            md.push_str(&format!(
                "| | server | {} | {} | {} | {} | {} |\n",
                fmt(c, "min"),
                fmt(c, "median"),
                fmt(c, "p95"),
                fmt(c, "max"),
                fmt(c, "stddev")
            ));
        } else {
            md.push_str("| | server | - | - | - | - | - |\n");
        }
    }
    if !skips.is_empty() {
        md.push_str("\n## Skipped\n\n");
        md.push_str(&skips.join("\n"));
        md.push('\n');
    }
    md
}
