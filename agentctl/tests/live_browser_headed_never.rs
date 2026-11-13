//! A visible (headed) Chrome driven under `foreground = "never"` must leave the
//! person's frontmost app alone.
//!
//! STATUS: run on macOS with an unlocked screen (3 of 3 passes: Finder stayed
//! frontmost while a visible Chrome was driven under `never`, attach mode).
//! Not measured: Safari, Linux, and attaching to a person's own everyday Chrome
//! (see `docs/threat-model.md`). What is verified without a screen is in `mcp-browser/src/foreground_live.rs`
//! (the commands the engine sends, read off a real session) and
//! `browser_foreground.rs` (the refusals).
//!
//! Gated like the other GUI suites: `AGENTCTL_SKIP_LIVE` unset,
//! `AGENTCTL_LIVE_GUI=1`, and a session that can take input.
//!
//! What it does: starts a throwaway headed Chrome with a profile of its own
//! (never the person's), asking `open` not to activate it (`-g`); brings Finder
//! to the front; then drives that browser through a server configured `never`
//! (attach, open a tab, navigate, focus, type, click, screenshot, evaluate)
//! while sampling the frontmost application every 50 ms. The sampler must see
//! Finder throughout and never Chrome. The raising operations (`tabs
//! activate`, launching a visible browser) must be refused. The throwaway
//! browser and its profile are removed at the end, matched by the unique
//! profile path, so no other Chrome is touched.

#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mcp_policy::{AuditSink, NoConsent};
use mcp_types::{Category, ErrorCode, Foreground};
use serde_json::json;
use test_support::{live_gui_enabled, test_policy, InProcClient};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const PAGE: &str = include_str!("../../docs/fixtures/focus_probe.html");

fn osascript(script: &str) -> Option<String> {
    let out = Command::new("/usr/bin/osascript")
        .args(["-e", script])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn active_app() -> Option<String> {
    osascript(
        "tell application \"System Events\" to get name of first application process \
         whose frontmost is true",
    )
}

fn chrome_app() -> Option<PathBuf> {
    let bin = std::path::Path::new("/Applications/Google Chrome.app");
    bin.exists().then(|| bin.to_path_buf())
}

/// A headed Chrome of our own, and the means to remove it. Dropping it kills
/// every process whose command line names our profile directory and nothing
/// else.
struct Throwaway {
    profile: PathBuf,
    port: u16,
}

impl Throwaway {
    fn start(app: &std::path::Path) -> Option<Throwaway> {
        let profile =
            std::env::temp_dir().join(format!("agentctl-headed-never-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&profile);
        std::fs::create_dir_all(&profile).ok()?;
        // `-g`: do not bring the app forward. `-n`: a new instance, so the
        // person's own Chrome is not reused or touched.
        let status = Command::new("/usr/bin/open")
            .args(["-g", "-n", "-a"])
            .arg(app)
            .arg("--args")
            .arg("--remote-debugging-port=0")
            .arg(format!("--user-data-dir={}", profile.display()))
            .args(["--no-first-run", "--no-default-browser-check"])
            .status()
            .ok()?;
        if !status.success() {
            return None;
        }
        let file = profile.join("DevToolsActivePort");
        let end = Instant::now() + Duration::from_secs(20);
        while Instant::now() < end {
            if let Some(port) = std::fs::read_to_string(&file)
                .ok()
                .and_then(|t| t.lines().next().and_then(|l| l.trim().parse::<u16>().ok()))
            {
                return Some(Throwaway { profile, port });
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let t = Throwaway { profile, port: 0 };
        drop(t);
        None
    }
}

impl Drop for Throwaway {
    fn drop(&mut self) {
        // The profile path alone: a pattern that starts with `--` is read by
        // pkill as an option, which left the browser running.
        let needle = self.profile.display().to_string();
        let _ = Command::new("/usr/bin/pkill")
            .args(["-f", &needle])
            .status();
        std::thread::sleep(Duration::from_millis(500));
        let _ = std::fs::remove_dir_all(&self.profile);
    }
}

async fn serve() -> (String, tokio::sync::oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut rx => break,
                res = listener.accept() => {
                    let Ok((mut stream, _)) = res else { continue };
                    tokio::spawn(async move {
                        let mut buf = [0u8; 2048];
                        let _ = stream.read(&mut buf).await;
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                            PAGE.len(),
                            PAGE
                        );
                        let _ = stream.write_all(resp.as_bytes()).await;
                    });
                }
            }
        }
    });
    (format!("http://127.0.0.1:{port}/"), tx)
}

/// Samples the frontmost application until stopped.
struct Watch {
    stop: Arc<AtomicBool>,
    seen: Arc<Mutex<Vec<String>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Watch {
    fn start() -> Watch {
        let stop = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let (s, v) = (stop.clone(), seen.clone());
        let thread = std::thread::spawn(move || {
            while !s.load(Ordering::SeqCst) {
                if let Some(a) = active_app() {
                    let mut g = v.lock().unwrap();
                    if g.last() != Some(&a) {
                        g.push(a);
                    }
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        Watch {
            stop,
            seen,
            thread: Some(thread),
        }
    }

    fn finish(mut self) -> Vec<String> {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.seen.lock().unwrap().clone()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_headed_browser_driven_under_never_leaves_the_frontmost_app_alone() {
    if !live_gui_enabled() {
        eprintln!("skipping: the GUI suite is off here (needs AGENTCTL_LIVE_GUI=1, AGENTCTL_SKIP_LIVE unset and an unlocked screen)");
        return;
    }
    let Some(app) = chrome_app() else {
        eprintln!("skipping: Google Chrome.app is not installed");
        return;
    };
    let Some(chrome) = Throwaway::start(&app) else {
        eprintln!("skipping: could not start a throwaway headed Chrome");
        return;
    };
    let (url, stop_server) = serve().await;

    // Whatever starting Chrome did, put Finder in front and require it.
    osascript("tell application \"Finder\" to activate");
    tokio::time::sleep(Duration::from_millis(800)).await;
    if active_app().as_deref() != Some("Finder") {
        eprintln!("skipping: could not bring Finder to the front to start from");
        return;
    }

    let mut cfg = test_policy(&[Category::Browser]);
    cfg.foreground = Foreground::Never;
    // The fixture is served from loopback.
    cfg.browser_allow_private = true;
    cfg.enable = vec!["browser_eval".to_string()];
    let server = agentctl::build_server(
        cfg,
        AuditSink::memory(),
        Arc::new(NoConsent),
        "headed-never",
    )
    .expect("server builds");
    let c = InProcClient::new(server);

    let watch = Watch::start();
    let connected = c
        .ok("browser_connect", json!({"attach": {"port": chrome.port}}))
        .await;
    assert_eq!(connected["foregrounded"], json!(false), "{connected}");
    let id = connected["browser_id"].as_u64().expect("browser_id");

    let opened = c
        .ok(
            "browser_tabs",
            json!({"browser_id": id, "action": "open", "url": url}),
        )
        .await;
    let tab = opened["target_id"].as_str().expect("target_id").to_string();
    tokio::time::sleep(Duration::from_millis(800)).await;
    for (query, action, value) in [
        ("#a", "focus", None),
        ("#a", "type", Some("hello")),
        ("#go", "click", None),
        ("#b", "focus", None),
    ] {
        let mut args = json!({"target_id": tab, "query": query, "action": action});
        if let Some(v) = value {
            args["value"] = json!(v);
        }
        c.ok("browser_act", args).await;
    }
    c.ok("browser_screenshot", json!({"target_id": tab})).await;
    let focused = c
        .ok(
            "browser_eval",
            json!({"target_id": tab, "expression": "document.hasFocus()"}),
        )
        .await;
    assert_eq!(focused["result"], json!(true), "{focused}");

    // The raising operations are refused, not performed.
    let env = c
        .call(
            "browser_tabs",
            json!({"browser_id": id, "action": "activate", "target_id": tab}),
        )
        .await;
    assert_eq!(env.error.expect("refused").code, ErrorCode::PolicyDenied);
    let env = c
        .call("browser_connect", json!({"launch": {"headless": false}}))
        .await;
    assert_eq!(env.error.expect("refused").code, ErrorCode::PolicyDenied);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let fronts = watch.finish();
    let _ = c
        .call("browser_disconnect", json!({"browser_id": id}))
        .await;
    let _ = stop_server.send(());
    assert_eq!(
        fronts,
        ["Finder"],
        "the frontmost application changed while a headed browser was driven: {fronts:?}"
    );
}
