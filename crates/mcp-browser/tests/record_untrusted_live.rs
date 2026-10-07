//! Live test (real headless Chrome): page script cannot fake input either.
//!
//! The recorder listens on the page's DOM, so page script can `click()` or
//! `dispatchEvent` at will. It records an event only when the browser made it
//! (`isTrusted`) or when agentctl armed it from the recorder's own world just
//! before dispatching it for a `browser_act` or `browser_fill_form` call.
//!
//! Skipped when `AGENTCTL_SKIP_LIVE` is set or no Chrome binary is found.

use std::sync::Arc;
use std::time::Duration;

use mcp_browser::{BrowserBackend, CdpBackend, Locator, NavPolicy, RecordManager, CHROME_BINS};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

mod common;

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

/// Serves `content` for every path.
async fn serve_html(content: String) -> (String, tokio::sync::oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut rx => break,
                res = listener.accept() => {
                    let Ok((mut stream, _)) = res else { continue };
                    let mut buf = [0u8; 1024];
                    let _ = stream.read(&mut buf).await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        content.len(),
                        content
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.flush().await;
                }
            }
        }
    });
    (format!("http://127.0.0.1:{port}"), tx)
}

async fn js(b: &CdpBackend, target: &str, expr: &str) -> Value {
    let env = b.eval(target, expr).await.expect("eval");
    env.get("result")
        .cloned()
        .expect("eval envelope has result")
}

async fn act(b: &CdpBackend, t: &str, css: &str, action: &str, value: Option<&str>) {
    b.act(
        t,
        Locator::Selector {
            by: "css",
            query: css,
            within: None,
            text: None,
            index: None,
        },
        action,
        value,
    )
    .await
    .unwrap_or_else(|e| panic!("act {action} on {css}: {e:?}"));
}

/// A page that attacks the recorder from its own script, on load and every
/// 50 ms: it clicks `#go`, fires `input`/`change` at `#trap` (after setting its
/// value) and at `#name`, and dispatches a synthetic Enter. Its click handler
/// on `#go` also re-dispatches a synthetic click on the same button, which is
/// what a page would do to land a second event inside the agent's own. The
/// same goes for `#deep`, a button inside an open shadow root on `#host`.
const PAGE: &str = r##"<!doctype html><html><body>
<input id="name"><input id="trap">
<select id="sel"><option value="a">A</option><option value="b">B</option></select>
<input id="agree" type="checkbox"><input id="email">
<button id="go">Go</button><button id="plain">Plain</button>
<div id="host"></div>
<canvas id="cv" width="200" height="60" data-canvas-regions='[{"id":"hit","x":0,"y":0,"w":200,"h":60}]'></canvas>
<script>
window.__ticks = 0;
var nested = false;
var deep = document.getElementById('host').attachShadow({mode: 'open'});
deep.innerHTML = '<button id="deep">Deep</button>';
deep.getElementById('deep').addEventListener('click', function () {
  if (nested) return;
  nested = true;
  deep.getElementById('deep').click();
  nested = false;
});
document.getElementById('go').addEventListener('click', function () {
  if (nested) return;
  nested = true;
  document.getElementById('go').dispatchEvent(new MouseEvent('click', {bubbles: true}));
  nested = false;
});
function attack() {
  window.__ticks++;
  document.getElementById('go').click();
  var trap = document.getElementById('trap');
  trap.value = 'evil';
  trap.dispatchEvent(new Event('input', {bubbles: true}));
  trap.dispatchEvent(new Event('change', {bubbles: true}));
  trap.dispatchEvent(new KeyboardEvent('keydown', {key: 'Enter', bubbles: true}));
  deep.getElementById('deep').click();
  var name = document.getElementById('name');
  name.dispatchEvent(new Event('input', {bubbles: true}));
  name.dispatchEvent(new Event('change', {bubbles: true}));
}
attack();
setInterval(attack, 50);
</script>
</body></html>"##;

fn seen(events: &[mcp_browser::RawInteractionEvent]) -> Vec<String> {
    events
        .iter()
        .filter(|e| e.kind != "navigate")
        .map(|e| {
            format!(
                "{}:{}:{}",
                e.kind,
                e.selector,
                e.value.as_deref().or(e.key.as_deref()).unwrap_or("")
            )
        })
        .collect()
}

async fn ticks(b: &CdpBackend, t: &str) -> i64 {
    js(b, t, "window.__ticks").await.as_i64().unwrap_or(0)
}

/// Wait for the page to have attacked at least `n` more times than `from`, so
/// the attacks overlap the step that follows rather than preceding it.
async fn attacked_since(b: &CdpBackend, t: &str, from: i64, n: i64) {
    common::wait_until(
        "the page's attack timer to fire",
        Duration::from_secs(10),
        || async { ticks(b, t).await >= from + n },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn page_synthetic_events_are_not_recorded_but_agent_and_real_input_are() {
    let Some((b, t)) = tab().await else {
        return;
    };
    let (base, stop) = serve_html(PAGE.to_string()).await;
    b.navigate(&t, "goto", Some(&base)).await.expect("navigate");
    RecordManager::start(b.as_ref(), &t).await.expect("start");
    let t0 = ticks(&b, &t).await;
    attacked_since(&b, &t, t0, 3).await;

    // Agent actions, each one while the page keeps attacking.
    act(&b, &t, "#name", "type", Some("Ada")).await;
    attacked_since(&b, &t, ticks(&b, &t).await, 2).await;
    act(&b, &t, "#sel", "select", Some("b")).await;
    act(&b, &t, "#go", "click", None).await;
    let fields = json!([
        { "selector": "#agree", "type": "checkbox", "value": true },
        { "selector": "#email", "value": "a@b.c" }
    ]);
    b.fill_form(&t, &fields, Some(&json!({ "selector": "#plain" })))
        .await
        .expect("fill_form");
    assert_eq!(
        js(&b, &t, "document.getElementById('agree').checked").await,
        true,
        "the agent's own action still takes effect while recording"
    );

    // Inside an open shadow root the event reaches the recorder retargeted
    // to the host; the agent's click must still be recognised as its own.
    let snap = b.snapshot(&t, "dom", None).await.expect("snapshot");
    let deep = snap["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .find(|n| n.get("name").and_then(Value::as_str) == Some("Deep"))
        .and_then(|n| n.get("ref").and_then(Value::as_str))
        .expect("shadow button node")
        .to_string();
    b.act(&t, Locator::Ref(&deep), "click", None)
        .await
        .expect("shadow click");

    // A real pointer click (CDP Input.dispatchMouseEvent on a canvas region)
    // and a real Enter key press (CDP Input.dispatchKeyEvent).
    let snap = b.snapshot(&t, "dom", None).await.expect("snapshot");
    let region = snap["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .find(|n| n.get("name").and_then(Value::as_str) == Some("hit"))
        .and_then(|n| n.get("ref").and_then(Value::as_str))
        .expect("canvas region node")
        .to_string();
    let res = b
        .act(&t, Locator::Ref(&region), "click", None)
        .await
        .expect("canvas click");
    assert_eq!(res.get("input").and_then(Value::as_str), Some("cdp"));
    act(&b, &t, "#name", "press", Some("Enter")).await;
    attacked_since(&b, &t, ticks(&b, &t).await, 3).await;

    let events = RecordManager::stop_raw(b.as_ref(), &t).await.expect("stop");
    let seen = seen(&events);
    let count = |s: &str| seen.iter().filter(|e| e.as_str() == s).count();

    // The agent's actions were recorded once each.
    assert_eq!(count("input:#name:Ada"), 1, "{seen:?}");
    // Typed for real, so the browser adds its own `change` when the real
    // click on #go takes focus away: the armed one plus that one.
    assert_eq!(count("change:#name:Ada"), 2, "{seen:?}");
    assert_eq!(count("change:#sel:b"), 1, "{seen:?}");
    assert_eq!(
        count("click:#go:"),
        1,
        "agent click once, no nested one: {seen:?}"
    );
    assert_eq!(count("change:#agree:on"), 1, "{seen:?}");
    assert_eq!(count("input:#email:a@b.c"), 1, "{seen:?}");
    assert_eq!(count("change:#email:a@b.c"), 1, "{seen:?}");
    assert_eq!(count("click:#plain:"), 1, "{seen:?}");
    assert_eq!(
        count("click:#host:"),
        1,
        "agent click in a shadow root: {seen:?}"
    );
    // Real input was recorded.
    assert_eq!(count("click:#cv:"), 1, "real canvas click: {seen:?}");
    assert_eq!(count("keydown:#name:Enter"), 1, "real Enter: {seen:?}");
    // Nothing else: none of the page's synthetic events got in.
    assert!(!seen.iter().any(|e| e.contains("#trap")), "{seen:?}");
    assert!(!seen.iter().any(|e| e.contains("evil")), "{seen:?}");
    assert_eq!(seen.len(), 13, "exactly the expected events: {seen:?}");

    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}
