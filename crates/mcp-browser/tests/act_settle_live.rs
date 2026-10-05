//! Live tests (real headless Chrome) for the waits that follow a `browser_act`
//! (`network_idle`, `htmx_settled`, `wait_after: "settle"`) and for the page not
//! being scrolled sideways by a click.
//!
//! The failure these guard: a click starts its request or navigation a moment
//! later, the old page is still loaded and quiet, so a wait reads the old page
//! and the flow concludes the app is broken. Skipped when `AGENTCTL_SKIP_LIVE`
//! is set or no Chrome binary is found.

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

async fn tab() -> Option<(Arc<CdpBackend>, String)> {
    if !have_chrome() {
        return None;
    }
    let b = CdpBackend::new(NavPolicy::new(&[], true));
    b.connect(None, Some(json!({ "headless": true, "port": 0 })))
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

/// How long `/data` holds its response. Longer than the 2 s a click is given
/// to start navigating, so only a wait that really watches the request can
/// still be waiting when it arrives.
const DATA_MS: u64 = 2_500;

/// - `/`: `#delayed` fetches `/data` 300 ms after the click and shows the
///   answer in `#out`; `#nav` links to `/next`.
/// - `/hx`: a page with a stand-in `window.htmx`; `#hx` raises htmx's request
///   events 600 ms after the click, and swaps `#out` 400 ms after that.
/// - `/wide`: a 2000 px wide page with a small `#b` button near the left.
/// - `/data`: answers `loaded` after `DATA_MS`. `/next`: a second page.
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
                        let body = if path.starts_with("/data") {
                            // Fixture behaviour, not a readiness wait: the
                            // endpoint is slow on purpose.
                            tokio::time::sleep(Duration::from_millis(DATA_MS)).await;
                            "loaded".to_string()
                        } else if path.starts_with("/next") {
                            "<!doctype html><body><h1 id=\"which\">next page</h1></body>".to_string()
                        } else if path.starts_with("/hx") {
                            "<!doctype html><body><h1 id=\"which\">hx page</h1>\
                             <div id=\"out\">idle</div>\
                             <button id=\"hx\" onclick=\"setTimeout(function(){ev('beforeRequest');setTimeout(function(){document.getElementById('out').textContent='swapped';ev('afterRequest');ev('afterSettle')},400)},600)\">hx</button>\
                             <script>window.htmx={};function ev(n){document.body.dispatchEvent(new CustomEvent('htmx:'+n,{bubbles:true}))}</script></body>"
                                .to_string()
                        } else if path.starts_with("/wide") {
                            "<!doctype html><body style=\"margin:0\"><div style=\"width:2000px;height:200px;position:relative\">\
                             <button id=\"b\" style=\"position:absolute;left:350px;top:50px;width:100px\">b</button></div></body>"
                                .to_string()
                        } else {
                            "<!doctype html><body><h1 id=\"which\">first page</h1>\
                             <div id=\"out\">idle</div>\
                             <button id=\"delayed\" onclick=\"setTimeout(function(){fetch('/data').then(function(r){return r.text()}).then(function(t){document.getElementById('out').textContent=t})},300)\">go</button>\
                             <a id=\"nav\" href=\"/next\">next</a></body>"
                                .to_string()
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

fn ctx() -> CallCtx {
    CallCtx::new("test", CancelToken::new())
}

async fn eval_str(b: &CdpBackend, t: &str, js: &str) -> Value {
    b.eval(t, js).await.expect("eval")["result"].clone()
}

async fn act(m: &BrowserModule, t: &str, query: &str, extra: Value) -> Envelope {
    let mut args = json!({ "target_id": t, "query": query, "action": "click" });
    for (k, v) in extra.as_object().into_iter().flatten() {
        args[k] = v.clone();
    }
    m.call("browser_act", args, &ctx()).await
}

async fn wait(m: &BrowserModule, t: &str, condition: &str) -> Envelope {
    m.call(
        "browser_wait",
        json!({ "target_id": t, "condition": condition, "timeout_ms": 15000 }),
        &ctx(),
    )
    .await
}

async fn open(b: &CdpBackend, t: &str, url: String) {
    b.navigate(t, "goto", Some(&url)).await.expect("goto");
}

/// A click that fetches 300 ms later, from a server that answers 2.5 s after
/// that. `network_idle` straight after the click used to settle on the old
/// page (still `complete`, still quiet); now it must wait out the request.
#[tokio::test(flavor = "multi_thread")]
async fn network_idle_after_a_click_waits_for_the_delayed_request() {
    let Some((b, t)) = tab().await else {
        return;
    };
    let (base, stop) = serve().await;
    open(&b, &t, format!("{base}/")).await;
    let m = BrowserModule::new(b.clone());

    let c = act(&m, &t, "#delayed", json!({})).await;
    assert!(c.ok, "{c:?}");
    let w = wait(&m, &t, "network_idle").await;
    assert!(w.ok, "{w:?}");
    assert_eq!(
        eval_str(&b, &t, "document.getElementById('out').textContent").await,
        "loaded",
        "network_idle returned before the request the click started had finished"
    );

    // With no action in between, an idle page settles promptly (not at the timeout).
    let started = Instant::now();
    let w = wait(&m, &t, "network_idle").await;
    assert!(w.ok, "{w:?}");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "an idle page took {:?} to settle",
        started.elapsed()
    );

    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

/// `browser_assert wait_network_idle` shares the probe.
#[tokio::test(flavor = "multi_thread")]
async fn assert_wait_network_idle_sees_the_delayed_request() {
    let Some((b, t)) = tab().await else {
        return;
    };
    let (base, stop) = serve().await;
    open(&b, &t, format!("{base}/")).await;
    let m = BrowserModule::new(b.clone());
    assert!(act(&m, &t, "#delayed", json!({})).await.ok);
    let a = m
        .call(
            "browser_assert",
            json!({
                "target_id": t,
                "wait_network_idle": true,
                "timeout_ms": 15000,
                "selector": "#out",
                "text": "loaded"
            }),
            &ctx(),
        )
        .await;
    assert!(a.ok, "{a:?}");
    assert_eq!(
        eval_str(&b, &t, "document.getElementById('out').textContent").await,
        "loaded"
    );
    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

/// `wait_after: "settle"` returns once the request has been answered, and says
/// what happened.
#[tokio::test(flavor = "multi_thread")]
async fn act_wait_after_settle_reports_what_happened() {
    let Some((b, t)) = tab().await else {
        return;
    };
    let (base, stop) = serve().await;
    open(&b, &t, format!("{base}/")).await;
    let m = BrowserModule::new(b.clone());

    // A request, no navigation.
    let c = act(
        &m,
        &t,
        "#delayed",
        json!({ "wait_after": "settle", "timeout_ms": 15000 }),
    )
    .await;
    assert!(c.ok, "{c:?}");
    let d = c.data.as_ref().unwrap();
    assert_eq!(d["settled"], true, "{d}");
    assert_eq!(d["navigated"], false, "{d}");
    assert_eq!(d["requests_started"], 1, "{d}");
    assert_eq!(
        eval_str(&b, &t, "document.getElementById('out').textContent").await,
        "loaded",
        "the act returned before the page had changed"
    );

    // A navigation: the new document has loaded when the act returns.
    let c = act(&m, &t, "#nav", json!({ "wait_after": "settle" })).await;
    assert!(c.ok, "{c:?}");
    let d = c.data.as_ref().unwrap();
    assert_eq!(d["settled"], true, "{d}");
    assert_eq!(d["navigated"], true, "{d}");
    assert_eq!(
        eval_str(&b, &t, "document.getElementById('which').textContent").await,
        "next page"
    );

    // Out of time: the click still happened, and the result says it did not settle.
    open(&b, &t, format!("{base}/")).await;
    let c = act(
        &m,
        &t,
        "#delayed",
        json!({ "wait_after": "settle", "timeout_ms": 1000 }),
    )
    .await;
    assert!(c.ok, "{c:?}");
    let d = c.data.as_ref().unwrap();
    assert_eq!(d["settled"], false, "{d}");
    assert!(d["settle_error"].is_string(), "{d}");
    assert_eq!(d["requests_started"], 1, "{d}");

    // Bad values are refused before anything is clicked.
    let bad = act(&m, &t, "#delayed", json!({ "wait_after": "later" })).await;
    assert!(!bad.ok, "{bad:?}");

    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

/// An htmx trigger that starts 600 ms after the click: `htmx_settled` right
/// after the act must wait for it rather than read the quiet page.
#[tokio::test(flavor = "multi_thread")]
async fn htmx_settled_after_a_click_waits_for_a_delayed_trigger() {
    let Some((b, t)) = tab().await else {
        return;
    };
    let (base, stop) = serve().await;
    open(&b, &t, format!("{base}/hx")).await;
    let m = BrowserModule::new(b.clone());

    let c = act(&m, &t, "#hx", json!({})).await;
    assert!(c.ok, "{c:?}");
    let w = wait(&m, &t, "htmx_settled").await;
    assert!(w.ok, "{w:?}");
    assert_eq!(
        eval_str(&b, &t, "document.getElementById('out').textContent").await,
        "swapped",
        "htmx_settled returned before the delayed request ran"
    );

    // And through wait_after.
    open(&b, &t, format!("{base}/hx")).await;
    let c = act(&m, &t, "#hx", json!({ "wait_after": "settle" })).await;
    assert!(c.ok, "{c:?}");
    assert_eq!(c.data.as_ref().unwrap()["settled"], true, "{c:?}");
    assert_eq!(
        eval_str(&b, &t, "document.getElementById('out').textContent").await,
        "swapped"
    );

    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

/// The click used to centre its target in both axes, which scrolls a wide page
/// sideways and leaves a blank strip on one side. Default is now `nearest`.
#[tokio::test(flavor = "multi_thread")]
async fn click_does_not_scroll_a_wide_page_sideways() {
    let Some((b, t)) = tab().await else {
        return;
    };
    let (base, stop) = serve().await;
    open(&b, &t, format!("{base}/wide")).await;
    b.set_viewport(&t, 600, 600, false, 1.0)
        .await
        .expect("viewport");
    // Right of the middle but fully inside the viewport, whatever its width
    // turns out to be: centring it must scroll, "nearest" must not.
    eval_str(
        &b,
        &t,
        "document.getElementById('b').style.left = (window.innerWidth * 0.7 - 50) + 'px'; true",
    )
    .await;
    let m = BrowserModule::new(b.clone());
    let scroll_x = || eval_str(&b, &t, "window.scrollX");

    assert_eq!(scroll_x().await, 0);
    // Visible already: the default and `none` leave the page where it is.
    let c = act(&m, &t, "#b", json!({})).await;
    assert!(c.ok, "{c:?}");
    assert_eq!(
        scroll_x().await,
        0,
        "a click on a visible button scrolled the page"
    );
    let c = act(&m, &t, "#b", json!({ "scroll": "none" })).await;
    assert!(c.ok, "{c:?}");
    assert_eq!(scroll_x().await, 0);
    // Centring does move it (this is what the old behaviour did).
    let c = act(&m, &t, "#b", json!({ "scroll": "center" })).await;
    assert!(c.ok, "{c:?}");
    let x = scroll_x().await.as_f64().unwrap();
    assert!(x > 20.0, "scroll:center left scrollX at {x}");

    // An unknown mode is refused.
    let bad = act(&m, &t, "#b", json!({ "scroll": "smooth" })).await;
    assert!(!bad.ok, "{bad:?}");

    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}
