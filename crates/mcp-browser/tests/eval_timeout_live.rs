//! Live tests (real headless Chrome) for `browser_eval`'s `timeout_ms` and
//! `detached` options, and for `browser_connect`'s `launch.args`.
//!
//! Skipped when `AGENTCTL_SKIP_LIVE` is set or no Chrome binary is found.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use mcp_browser::{BrowserBackend, BrowserModule, CdpBackend, NavPolicy, CHROME_BINS};
use mcp_types::{CallCtx, CancelToken, Envelope, ToolModule};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn skip_live() -> bool {
    std::env::var_os("AGENTCTL_SKIP_LIVE").is_some_and(|v| v != "0")
}

fn have_chrome() -> bool {
    !skip_live() && CHROME_BINS.iter().any(|p| std::path::Path::new(p).exists())
}

fn ctx() -> CallCtx {
    CallCtx::new("test", CancelToken::new())
}

async fn first_tab(b: &CdpBackend) -> Option<String> {
    let tabs = b.tabs(1, "list", None, None).await.ok()?;
    Some(
        tabs.get("tabs")?
            .as_array()?
            .first()?
            .get("target_id")?
            .as_str()?
            .to_string(),
    )
}

async fn tab() -> Option<(Arc<CdpBackend>, String)> {
    if !have_chrome() {
        return None;
    }
    let b = CdpBackend::new(NavPolicy::new(&[], true));
    b.connect(None, Some(json!({ "headless": true, "port": 0 })))
        .await
        .ok()?;
    let t = first_tab(&b).await?;
    Some((Arc::new(b), t))
}

/// Two static pages: `/` and `/next`.
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
                        let n = stream.read(&mut buf).await.unwrap_or(0);
                        let req = String::from_utf8_lossy(&buf[..n]).to_string();
                        let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                        let body = if path.starts_with("/next") {
                            "<!doctype html><body><h1 id=\"which\">next page</h1></body>"
                        } else {
                            "<!doctype html><body><h1 id=\"which\">first page</h1></body>"
                        };
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(resp.as_bytes()).await;
                        let _ = stream.flush().await;
                    });
                }
            }
        }
    });
    (format!("http://127.0.0.1:{port}"), tx)
}

async fn eval(m: &BrowserModule, t: &str, args: Value) -> Envelope {
    let mut a = json!({ "target_id": t });
    for (k, v) in args.as_object().expect("object") {
        a[k] = v.clone();
    }
    m.call("browser_eval", a, &ctx()).await
}

fn err_text(e: &Envelope) -> String {
    serde_json::to_string(&e.error).unwrap_or_default()
}

/// An endless synchronous loop must come back as a timeout, say plainly what
/// was and was not stopped, and leave the tab usable: the follow-up eval only
/// runs if the loop really was terminated, not merely abandoned.
#[tokio::test(flavor = "multi_thread")]
async fn endless_sync_loop_times_out_and_is_terminated() {
    let Some((b, t)) = tab().await else {
        return;
    };
    let m = BrowserModule::new(b.clone());
    let started = Instant::now();
    let r = eval(
        &m,
        &t,
        json!({ "expression": "while(true){}", "timeout_ms": 500 }),
    )
    .await;
    let took = started.elapsed();
    assert!(!r.ok, "{r:?}");
    let text = err_text(&r);
    assert!(text.contains("timed out after 500 ms"), "{text}");
    assert!(text.contains("may still be running"), "{text}");
    assert!(took < Duration::from_secs(5), "took {took:?}");

    let next = eval(&m, &t, json!({ "expression": "1 + 1", "timeout_ms": 2000 })).await;
    assert!(next.ok, "{next:?}");
    assert_eq!(next.data.expect("data")["result"], json!(2));
    let _ = b.disconnect(1, true).await;
}

/// A script awaiting something that never settles times out at the transport
/// deadline, reports that termination was attempted, and the tab still works.
#[tokio::test(flavor = "multi_thread")]
async fn never_settling_promise_times_out_and_reports_termination_attempt() {
    let Some((b, t)) = tab().await else {
        return;
    };
    let m = BrowserModule::new(b.clone());
    let r = eval(
        &m,
        &t,
        json!({ "expression": "new Promise(function(){})", "timeout_ms": 300 }),
    )
    .await;
    assert!(!r.ok, "{r:?}");
    let text = err_text(&r);
    assert!(text.contains("timed out after 300 ms"), "{text}");
    assert!(text.contains("terminateExecution"), "{text}");
    assert!(text.contains("may still be running"), "{text}");

    let next = eval(&m, &t, json!({ "expression": "'alive'" })).await;
    assert!(next.ok, "{next:?}");
    assert_eq!(next.data.expect("data")["result"], json!("alive"));
    let _ = b.disconnect(1, true).await;
}

/// A script that navigates the page is not a failure: Chrome drops its result
/// ("Inspected target navigated or closed") and we report that it navigated.
#[tokio::test(flavor = "multi_thread")]
async fn script_that_navigates_reports_navigated_not_failure() {
    let Some((b, t)) = tab().await else {
        return;
    };
    let (base, stop) = serve().await;
    b.navigate(&t, "goto", Some(&format!("{base}/")))
        .await
        .expect("first page");
    let m = BrowserModule::new(b.clone());
    let r = eval(
        &m,
        &t,
        json!({ "expression": "new Promise(function(){ location.href = '/next'; })" }),
    )
    .await;
    assert!(r.ok, "{r:?}");
    let d = r.data.expect("data");
    assert_eq!(d["navigated"], json!(true), "{d}");
    assert_eq!(d["value"], Value::Null);

    common::wait_until("the new page", Duration::from_secs(10), || async {
        b.eval(
            &t,
            "document.getElementById('which') && document.getElementById('which').textContent",
        )
        .await
        .map(|v| v["result"] == json!("next page"))
        .unwrap_or(false)
    })
    .await;
    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

/// `detached` returns at once, even for a script that would take longer than
/// the call is willing to wait, and the script still runs.
#[tokio::test(flavor = "multi_thread")]
async fn detached_returns_immediately_and_script_still_runs() {
    let Some((b, t)) = tab().await else {
        return;
    };
    let m = BrowserModule::new(b.clone());
    let started = Instant::now();
    let r = eval(
        &m,
        &t,
        json!({
            "expression": "new Promise(function(r){ setTimeout(function(){ window.__detached = 'done'; r(); }, 1500); })",
            "detached": true
        }),
    )
    .await;
    let took = started.elapsed();
    assert!(r.ok, "{r:?}");
    assert_eq!(r.data.expect("data")["started"], json!(true));
    assert!(took < Duration::from_millis(1200), "took {took:?}");
    let before = b.eval(&t, "window.__detached || null").await.expect("eval");
    assert_eq!(before["result"], Value::Null, "script ran synchronously");

    common::wait_until(
        "the detached script to finish",
        Duration::from_secs(10),
        || async {
            b.eval(&t, "window.__detached || null")
                .await
                .map(|v| v["result"] == json!("done"))
                .unwrap_or(false)
        },
    )
    .await;
    let _ = b.disconnect(1, true).await;
}

/// `launch.args` reach Chrome (`--user-agent` changes `navigator.userAgent`), and a
/// denied flag is refused before anything is launched.
#[tokio::test(flavor = "multi_thread")]
async fn launch_args_are_passed_and_denied_flags_refused() {
    if !have_chrome() {
        return;
    }
    let b = Arc::new(CdpBackend::new(NavPolicy::new(&[], true)));
    let m = BrowserModule::new(b.clone());
    let denied = m
        .call(
            "browser_connect",
            json!({ "launch": { "headless": true, "args": ["--no-sandbox"] } }),
            &ctx(),
        )
        .await;
    assert!(!denied.ok, "{denied:?}");
    assert!(err_text(&denied).contains("--no-sandbox"), "{denied:?}");
    assert!(
        b.tabs(1, "list", None, None).await.is_err(),
        "a browser was launched"
    );

    let ok = m
        .call(
            "browser_connect",
            json!({ "launch": { "headless": true, "args": ["--user-agent=agentctl-eval-test"] } }),
            &ctx(),
        )
        .await;
    assert!(ok.ok, "{ok:?}");
    assert!(ok
        .data
        .as_ref()
        .expect("data")
        .get("foregrounded")
        .is_some());
    let t = first_tab(&b).await.expect("tab");
    let ua = b.eval(&t, "navigator.userAgent").await.expect("eval");
    assert_eq!(ua["result"], json!("agentctl-eval-test"));
    let _ = b.disconnect(1, true).await;
}
