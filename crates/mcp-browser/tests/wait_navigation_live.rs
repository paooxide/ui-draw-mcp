//! Live tests (real headless Chrome) for `browser_wait navigation`.
//!
//! Skipped when `AGENTCTL_SKIP_LIVE` is set or no Chrome binary is found.
//! CDP ports 9503-9505 and 9514-9516.

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

/// A small site for history navigation and late navigations. Nothing is
/// cached, so going back or forward really asks the server again.
///
/// - `/p/<name>`, `/q/<name>`: a page whose `h1#which` is `page <name>`, with a
///   `#noop` button that does nothing. The first request for a path is
///   answered at once; every later one (a back or forward to it) after
///   `SLOW_MS` for `/p/`, and after `REVISIT_LONG_MS` for `/q/`.
/// - `/spa`: a page with `#push`, which `history.pushState`s `/spa/pushed`.
/// - `/late`: a page with `#late`, which navigates to `/target` three seconds
///   after the click. `/target` is `target page`.
async fn serve_site() -> (String, tokio::sync::oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let seen = Arc::new(std::sync::Mutex::new(
        std::collections::HashSet::<String>::new(),
    ));
    let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut rx => break,
                res = listener.accept() => {
                    let Ok((mut stream, _)) = res else { continue };
                    let seen = seen.clone();
                    tokio::spawn(async move {
                        let mut buf = [0u8; 2048];
                        let n = stream.read(&mut buf).await.unwrap_or(0);
                        let req = String::from_utf8_lossy(&buf[..n]).to_string();
                        let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                        let page = path.strip_prefix("/p/").or_else(|| path.strip_prefix("/q/"));
                        let body = if let Some(name) = page {
                            let revisit = !seen.lock().unwrap().insert(path.clone());
                            if revisit {
                                let ms = if path.starts_with("/q/") {
                                    REVISIT_LONG_MS
                                } else {
                                    SLOW_MS
                                };
                                tokio::time::sleep(Duration::from_millis(ms)).await;
                            }
                            format!(
                                "<!doctype html><body><h1 id=\"which\">page {name}</h1>\
                                 <button id=\"noop\">noop</button></body>"
                            )
                        } else if path.starts_with("/spa") {
                            "<!doctype html><body><h1 id=\"which\">spa</h1>\
                             <button id=\"push\" onclick=\"history.pushState({}, '', '/spa/pushed')\">push</button></body>"
                                .to_string()
                        } else if path.starts_with("/late") {
                            "<!doctype html><body><h1 id=\"which\">late page</h1>\
                             <button id=\"late\" onclick=\"setTimeout(function(){location.href='/target'},3000)\">late</button></body>"
                                .to_string()
                        } else {
                            "<!doctype html><body><h1 id=\"which\">target page</h1></body>".to_string()
                        };
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
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

/// A revisit slower than the 2 s window a click that went nowhere is given.
const REVISIT_LONG_MS: u64 = 3_000;

async fn go(m: &BrowserModule, t: &str, action: &str, url: Option<String>) {
    let mut args = json!({ "target_id": t, "action": action });
    if let Some(u) = url {
        args["url"] = json!(u);
    }
    let r = m.call("browser_navigate", args, &ctx()).await;
    assert!(r.ok, "{action}: {r:?}");
}

async fn wait_nav(m: &BrowserModule, t: &str, extra: Value) -> mcp_types::Envelope {
    let mut args = json!({ "target_id": t, "condition": "navigation", "timeout_ms": 10000 });
    for (k, v) in extra.as_object().into_iter().flatten() {
        args[k] = v.clone();
    }
    m.call("browser_wait", args, &ctx()).await
}

/// `back` and `forward` leave the document the way `goto` does, so a wait that
/// follows must wait for the history entry's page. A click that went nowhere
/// has left its own (uncertain) marker on the page, and the revisit is slower
/// than that marker's 2 s window, so a wait that settled for the old page
/// would show as `navigated: false` or the wrong page. A regression guard in
/// the way the goto one is: on the Chrome this was written against the first
/// evaluation after a history navigation is held until the new document
/// commits, so this passes with or without the marker back/forward now plant.
#[tokio::test(flavor = "multi_thread")]
async fn wait_navigation_after_back_and_forward_waits_for_the_history_entry() {
    let Some((b, t)) = tab(9514).await else {
        return;
    };
    let (base, stop) = serve_site().await;
    let m = BrowserModule::new(b.clone());
    let noop = || async {
        let c = m
            .call(
                "browser_act",
                json!({ "target_id": t, "query": "#noop", "action": "click" }),
                &ctx(),
            )
            .await;
        assert!(c.ok, "{c:?}");
    };
    go(&m, &t, "goto", Some(format!("{base}/q/a"))).await;
    assert!(wait_nav(&m, &t, json!({})).await.ok);
    go(&m, &t, "goto", Some(format!("{base}/q/b"))).await;
    assert!(wait_nav(&m, &t, json!({})).await.ok);
    assert_eq!(which(&b, &t).await, "page b");

    noop().await;
    let started = Instant::now();
    go(&m, &t, "back", None).await;
    let w = wait_nav(&m, &t, json!({})).await;
    let elapsed = started.elapsed();
    assert!(w.ok, "{w:?}");
    assert_eq!(
        which(&b, &t).await,
        "page a",
        "back: the wait returned on the page being left, after {elapsed:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(REVISIT_LONG_MS - 100),
        "back: wait returned after {elapsed:?}, before the slow response could have arrived"
    );

    noop().await;
    let started = Instant::now();
    go(&m, &t, "forward", None).await;
    let w = wait_nav(&m, &t, json!({})).await;
    let elapsed = started.elapsed();
    assert!(w.ok, "{w:?}");
    assert_eq!(
        which(&b, &t).await,
        "page b",
        "forward: the wait returned on the page being left, after {elapsed:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(REVISIT_LONG_MS - 100),
        "forward: wait returned after {elapsed:?}"
    );

    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

/// A history entry made by `pushState` keeps the document, so there is no new
/// one to wait for: the wait must settle at once, not run out its timeout
/// waiting for a replacement that will never come.
#[tokio::test(flavor = "multi_thread")]
async fn wait_navigation_after_back_within_the_same_document_settles() {
    let Some((b, t)) = tab(9515).await else {
        return;
    };
    let (base, stop) = serve_site().await;
    let m = BrowserModule::new(b.clone());
    go(&m, &t, "goto", Some(format!("{base}/spa"))).await;
    assert!(wait_nav(&m, &t, json!({})).await.ok);
    let c = m
        .call(
            "browser_act",
            json!({ "target_id": t, "query": "#push", "action": "click" }),
            &ctx(),
        )
        .await;
    assert!(c.ok, "{c:?}");
    let pushed = b.eval(&t, "location.pathname").await.expect("eval");
    assert_eq!(pushed["result"], "/spa/pushed");

    go(&m, &t, "back", None).await;
    let started = Instant::now();
    let w = wait_nav(&m, &t, json!({})).await;
    assert!(w.ok, "{w:?}");
    assert!(
        started.elapsed() < Duration::from_millis(2500),
        "waited {:?} for a document that was never going to be replaced",
        started.elapsed()
    );
    let path = b.eval(&t, "location.pathname").await.expect("eval");
    assert_eq!(path["result"], "/spa");

    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

/// A click whose handler navigates after 3 s is past the default 2 s window,
/// so the wait settles on the old page; with the window raised to 5 s it
/// waits for the navigation and reports it.
#[tokio::test(flavor = "multi_thread")]
async fn wait_navigation_window_can_be_raised_for_a_late_navigation() {
    let Some((b, t)) = tab(9516).await else {
        return;
    };
    let (base, stop) = serve_site().await;
    let m = BrowserModule::new(b.clone());

    // Raised to 5 s: the 3 s navigation is waited for.
    go(&m, &t, "goto", Some(format!("{base}/late"))).await;
    assert!(wait_nav(&m, &t, json!({})).await.ok);
    let c = m
        .call(
            "browser_act",
            json!({ "target_id": t, "query": "#late", "action": "click" }),
            &ctx(),
        )
        .await;
    assert!(c.ok, "{c:?}");
    let started = Instant::now();
    let w = wait_nav(&m, &t, json!({ "navigation_timeout_ms": 5000 })).await;
    let elapsed = started.elapsed();
    assert!(w.ok, "{w:?}");
    assert_eq!(w.data.as_ref().unwrap()["navigated"], true, "{w:?}");
    assert_eq!(
        which(&b, &t).await,
        "target page",
        "returned on the old page after {elapsed:?}"
    );
    assert!(elapsed >= Duration::from_millis(2500), "{elapsed:?}");

    // Default window: the same click settles on the old page at about 2 s.
    go(&m, &t, "goto", Some(format!("{base}/late"))).await;
    assert!(wait_nav(&m, &t, json!({})).await.ok);
    let c = m
        .call(
            "browser_act",
            json!({ "target_id": t, "query": "#late", "action": "click" }),
            &ctx(),
        )
        .await;
    assert!(c.ok, "{c:?}");
    let started = Instant::now();
    let w = wait_nav(&m, &t, json!({})).await;
    assert!(w.ok, "{w:?}");
    assert_eq!(w.data.as_ref().unwrap()["navigated"], false, "{w:?}");
    assert!(started.elapsed() < Duration::from_millis(2900));
    assert_eq!(which(&b, &t).await, "late page");

    // Out-of-range and misplaced values are refused, not guessed at.
    let too_big = wait_nav(&m, &t, json!({ "navigation_timeout_ms": 31000 })).await;
    assert!(!too_big.ok, "{too_big:?}");
    let wrong_cond = m
        .call(
            "browser_wait",
            json!({ "target_id": t, "selector": "h1", "navigation_timeout_ms": 1000 }),
            &ctx(),
        )
        .await;
    assert!(!wrong_cond.ok, "{wrong_cond:?}");

    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}
