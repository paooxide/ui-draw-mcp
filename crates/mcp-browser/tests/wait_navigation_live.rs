//! Live tests (real headless Chrome) for `browser_wait navigation`.
//!
//! Skipped when `AGENTCTL_SKIP_LIVE` is set or no Chrome binary is found.
//! CDP ports 9500-9509 only (this file uses 9503, 9504 and 9505).

use std::sync::Arc;
use std::time::{Duration, Instant};

use mcp_browser::{BrowserBackend, BrowserModule, CdpBackend, NavPolicy, CHROME_BINS};
use mcp_types::{CallCtx, CancelToken, ToolModule};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn skip_live() -> bool {
    std::env::var_os("AGENTCTL_SKIP_LIVE").is_some_and(|v| v != "0")
}

fn have_chrome() -> bool {
    !skip_live() && CHROME_BINS.iter().any(|p| std::path::Path::new(p).exists())
}

async fn tab(port: u64) -> Option<(Arc<CdpBackend>, String)> {
    if !have_chrome() {
        return None;
    }
    let b = CdpBackend::new(NavPolicy::new(&[], true));
    b.connect(None, Some(json!({ "headless": true, "port": port })))
        .await
        .ok()?;
    let tabs = b.tabs(1, "list", None, None).await.ok()?;
    let target = tabs
        .get("tabs")?
        .as_array()?
        .first()?
        .get("target_id")?
        .as_str()?
        .to_string();
    Some((Arc::new(b), target))
}

/// A tiny HTTP server. `/slow` waits `delay_ms` before it sends anything;
/// every other path answers at once. Each connection gets its own task, so a
/// slow response never holds up a fast one.
async fn serve(delay_ms: u64) -> (String, tokio::sync::oneshot::Sender<()>) {
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
                        let path = req
                            .split_whitespace()
                            .nth(1)
                            .unwrap_or("/")
                            .to_string();
                        let body = if path.starts_with("/slow") {
                            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                            "<!doctype html><body><h1 id=\"which\">slow page</h1></body>"
                        } else {
                            "<!doctype html><body><h1 id=\"which\">first page</h1>\
                             <a id=\"go\" href=\"/slow\">go slow</a>\
                             <button id=\"defer\" onclick=\"setTimeout(function(){location.href='/slow'},100)\">defer</button></body>"
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

fn ctx() -> CallCtx {
    CallCtx::new("test", CancelToken::new())
}

async fn which(b: &CdpBackend, t: &str) -> Value {
    let env = b
        .eval(t, "document.getElementById('which').textContent")
        .await
        .expect("eval");
    env["result"].clone()
}

const SLOW_MS: u64 = 800;

/// A wait after `goto` must not return before the new page has loaded. With
/// the server holding the response for 800 ms the wait has to take at least
/// that long, and the new page must be there after. A regression guard: on the
/// Chrome this was written against, `Page.navigate` itself blocks until the
/// new document commits, so this passes with or without the document marker.
#[tokio::test(flavor = "multi_thread")]
async fn wait_navigation_after_goto_does_not_return_on_the_old_document() {
    let Some((b, t)) = tab(9503).await else {
        return;
    };
    let (base, stop) = serve(SLOW_MS).await;
    b.navigate(&t, "goto", Some(&format!("{base}/")))
        .await
        .expect("first page");
    assert_eq!(which(&b, &t).await, "first page");

    let m = BrowserModule::new(b.clone());
    let started = Instant::now();
    let nav = m
        .call(
            "browser_navigate",
            json!({ "target_id": t, "action": "goto", "url": format!("{base}/slow") }),
            &ctx(),
        )
        .await;
    assert!(nav.ok, "{nav:?}");
    let w = m
        .call(
            "browser_wait",
            json!({ "target_id": t, "condition": "navigation", "timeout_ms": 10000 }),
            &ctx(),
        )
        .await;
    let elapsed = started.elapsed();
    assert!(w.ok, "{w:?}");
    assert_eq!(
        which(&b, &t).await,
        "slow page",
        "the wait returned while the old document was still showing (after {elapsed:?})"
    );
    assert!(
        elapsed >= Duration::from_millis(SLOW_MS - 100),
        "wait returned after {elapsed:?}, before the {SLOW_MS} ms response could have arrived"
    );

    // Once the navigation has been waited for, a second wait is the plain
    // "is it loaded" check and must not hang on a marker left behind.
    let again = Instant::now();
    let w = m
        .call(
            "browser_wait",
            json!({ "target_id": t, "condition": "navigation", "timeout_ms": 5000 }),
            &ctx(),
        )
        .await;
    assert!(w.ok, "{w:?}");
    assert!(again.elapsed() < Duration::from_millis(1500));

    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

/// The same after a *click* that navigates, which is how a recorded flow moves
/// between pages. Also a guard: Chrome holds the first evaluation after the
/// click until the navigation commits, so it passes without the marker too.
#[tokio::test(flavor = "multi_thread")]
async fn wait_navigation_after_a_click_waits_for_the_new_document() {
    let Some((b, t)) = tab(9504).await else {
        return;
    };
    let (base, stop) = serve(SLOW_MS).await;
    b.navigate(&t, "goto", Some(&format!("{base}/")))
        .await
        .expect("first page");

    let m = BrowserModule::new(b.clone());
    let started = Instant::now();
    let c = m
        .call(
            "browser_act",
            json!({ "target_id": t, "query": "#go", "action": "click" }),
            &ctx(),
        )
        .await;
    assert!(c.ok, "{c:?}");
    let w = m
        .call(
            "browser_wait",
            json!({ "target_id": t, "condition": "navigation", "timeout_ms": 10000 }),
            &ctx(),
        )
        .await;
    let elapsed = started.elapsed();
    assert!(w.ok, "{w:?}");
    assert_eq!(
        which(&b, &t).await,
        "slow page",
        "returned on the old page after {elapsed:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(SLOW_MS - 100),
        "{elapsed:?}"
    );

    // A click that does not navigate must not make the next wait hang: with
    // nothing loading, `navigation` settles on the loaded page.
    let none = m
        .call(
            "browser_act",
            json!({ "target_id": t, "query": "h1", "action": "click" }),
            &ctx(),
        )
        .await;
    assert!(none.ok, "{none:?}");
    let again = Instant::now();
    let w = m
        .call(
            "browser_wait",
            json!({ "target_id": t, "condition": "navigation", "timeout_ms": 10000 }),
            &ctx(),
        )
        .await;
    assert!(w.ok, "{w:?}");
    assert!(
        again.elapsed() < Duration::from_millis(6000),
        "a wait after a click that went nowhere took {:?}",
        again.elapsed()
    );

    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

/// The race that is real: a handler that starts the navigation a moment after
/// the click (a timer, a debounce, an analytics beacon first). The old page is
/// `complete` when the wait first looks, so a wait that only reads
/// `readyState` returns on it and the flow carries on against the wrong page.
#[tokio::test(flavor = "multi_thread")]
async fn wait_navigation_after_a_deferred_navigation_waits_for_the_new_page() {
    let Some((b, t)) = tab(9505).await else {
        return;
    };
    let (base, stop) = serve(SLOW_MS).await;
    b.navigate(&t, "goto", Some(&format!("{base}/")))
        .await
        .expect("first page");
    assert_eq!(which(&b, &t).await, "first page");

    let m = BrowserModule::new(b.clone());
    let c = m
        .call(
            "browser_act",
            json!({ "target_id": t, "query": "#defer", "action": "click" }),
            &ctx(),
        )
        .await;
    assert!(c.ok, "{c:?}");
    let w = m
        .call(
            "browser_wait",
            json!({ "target_id": t, "condition": "navigation", "timeout_ms": 10000 }),
            &ctx(),
        )
        .await;
    assert!(w.ok, "{w:?}");
    assert_eq!(
        which(&b, &t).await,
        "slow page",
        "the wait returned on the page the click left"
    );
    assert_eq!(w.data.as_ref().unwrap()["navigated"], true);

    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}
