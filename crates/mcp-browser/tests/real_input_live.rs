//! Live tests (real headless Chrome) for `browser_act` input that the page
//! cannot tell from a person's: a click with the whole pointer sequence, keys,
//! and text entry that a React-style controlled field accepts.
//!
//! The failures these guard: react-select opens its menu on `mousedown`, which
//! `el.click()` never fires; and a controlled field ignores an `input` event
//! when its value tracker already holds the new value, which `el.value = x`
//! guarantees. Skipped when `AGENTCTL_SKIP_LIVE` is set or no Chrome binary is
//! found.

use std::sync::Arc;

use mcp_browser::{BrowserBackend, BrowserModule, CdpBackend, NavPolicy, CHROME_BINS};
use mcp_types::ToolModule;
use mcp_types::{CallCtx, CancelToken, Envelope};
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

/// One page:
/// - `#menu` opens (`#menu-state` becomes `open`) only on a *trusted*
///   `mousedown`, like react-select; `#menu-state` says `untrusted` for a
///   synthetic one.
/// - `#covered` is under a full-size overlay; its `click` handler sets `#out`.
/// - `#alerter` raises an `alert()` when clicked.
/// - `#file` is a file input whose clicks are counted in `#out`.
/// - `#rx` and `#pw` stand in for React-controlled inputs: each has an own
///   `value` accessor that records the last value set through it, and an
///   `input` listener that only publishes the field to `#state` when the
///   value now differs from the recorded one (React's tracker rule).
/// - `#lb` is a listbox whose arrow keys move `#sel`.
const PAGE: &str = r#"<!doctype html><body style="margin:20px">
<div id="menu" style="padding:12px;border:1px solid #888;width:200px">select...</div>
<div id="menu-state">closed</div>
<div style="position:relative;width:200px;height:40px;margin-top:10px">
  <button id="covered" style="width:200px;height:40px">covered</button>
  <div id="overlay" style="position:absolute;left:0;top:0;width:200px;height:40px;background:rgba(0,0,0,.1)"></div>
</div>
<button id="alerter" onclick="alert('hello from a click')">alert</button>
<input id="file" type="file">
<div id="out"></div>
<input id="rx"><input id="pw" type="password"><div id="state"></div>
<div id="lb" tabindex="0" style="border:1px solid #888;width:100px;padding:4px">list</div>
<div id="sel">0</div>
<script>
var $ = function(i){ return document.getElementById(i); };
$('menu').addEventListener('mousedown', function(e){
  $('menu-state').textContent = e.isTrusted ? 'open' : 'untrusted';
});
$('covered').addEventListener('click', function(){ $('out').textContent = 'covered-clicked'; });
$('file').addEventListener('click', function(){ $('out').textContent = 'file-clicked'; });
function controlled(el){
  var d = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value');
  var tracked = '';
  Object.defineProperty(el, 'value', {
    configurable: true,
    get: function(){ return d.get.call(el); },
    set: function(v){ tracked = String(v); d.set.call(el, v); }
  });
  el.addEventListener('input', function(){
    var cur = d.get.call(el);
    if(cur === tracked) return;
    tracked = cur;
    $('state').textContent = el.id + '=' + cur;
  });
}
controlled($('rx')); controlled($('pw'));
$('lb').addEventListener('keydown', function(e){
  var n = +$('sel').textContent;
  if(e.key === 'ArrowDown') n++; else if(e.key === 'ArrowUp') n--; else return;
  $('sel').textContent = String(n);
  e.preventDefault();
});
</script></body>"#;

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
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            PAGE.len(),
                            PAGE
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

async fn text(b: &CdpBackend, t: &str, id: &str) -> Value {
    b.eval(t, &format!("document.getElementById('{id}').textContent"))
        .await
        .expect("eval")["result"]
        .clone()
}

async fn act(m: &BrowserModule, t: &str, query: &str, action: &str, extra: Value) -> Envelope {
    let mut args = json!({ "target_id": t, "query": query, "action": action });
    for (k, v) in extra.as_object().into_iter().flatten() {
        args[k] = v.clone();
    }
    m.call("browser_act", args, &ctx()).await
}

async fn setup() -> Option<(
    Arc<CdpBackend>,
    String,
    BrowserModule,
    tokio::sync::oneshot::Sender<()>,
)> {
    let (b, t) = tab().await?;
    let (base, stop) = serve().await;
    b.navigate(&t, "goto", Some(&format!("{base}/")))
        .await
        .expect("goto");
    let m = BrowserModule::new(b.clone());
    Some((b, t, m, stop))
}

/// A click reaches a mousedown-only widget as the browser's own event.
#[tokio::test(flavor = "multi_thread")]
async fn click_is_a_real_pointer_sequence() {
    let Some((b, t, m, stop)) = setup().await else {
        return;
    };
    let c = act(&m, &t, "#menu", "click", json!({})).await;
    assert!(c.ok, "{c:?}");
    assert_eq!(c.data.as_ref().unwrap()["input"], "cdp", "{c:?}");
    assert_eq!(text(&b, &t, "menu-state").await, "open");
    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

/// Where something covers the element, or a real click is unsafe, the click
/// stays `el.click()` and says why.
#[tokio::test(flavor = "multi_thread")]
async fn click_falls_back_to_synthetic_and_says_why() {
    let Some((b, t, m, stop)) = setup().await else {
        return;
    };
    let c = act(&m, &t, "#covered", "click", json!({})).await;
    assert!(c.ok, "{c:?}");
    let d = c.data.as_ref().unwrap();
    assert_eq!(d["input"], "synthetic", "{d}");
    assert!(
        d["input_reason"].as_str().unwrap().contains("covered"),
        "{d}"
    );
    assert_eq!(text(&b, &t, "out").await, "covered-clicked");

    // A real click on a file input would open the OS chooser.
    let f = act(&m, &t, "#file", "click", json!({})).await;
    assert!(f.ok, "{f:?}");
    let d = f.data.as_ref().unwrap();
    assert_eq!(d["input"], "synthetic", "{d}");
    assert!(d["input_reason"].as_str().unwrap().contains("file"), "{d}");
    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

/// A real click that raises `alert()` returns, and says what was asked.
#[tokio::test(flavor = "multi_thread")]
async fn a_real_click_that_raises_a_dialog_does_not_hang() {
    let Some((b, t, m, stop)) = setup().await else {
        return;
    };
    let c = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        act(&m, &t, "#alerter", "click", json!({})),
    )
    .await
    .expect("the click hung on the dialog");
    assert!(c.ok, "{c:?}");
    let d = c.data.as_ref().unwrap();
    assert_eq!(d["input"], "cdp", "{d}");
    assert_eq!(d["dialogs"][0]["message"], "hello from a click", "{d}");
    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

/// `type` and `fill_form` both reach a field whose tracker rejects an
/// assignment through its own accessor; a password is never echoed.
#[tokio::test(flavor = "multi_thread")]
async fn type_and_fill_form_reach_a_controlled_field() {
    let Some((b, t, m, stop)) = setup().await else {
        return;
    };
    let c = act(&m, &t, "#rx", "type", json!({ "value": "first" })).await;
    assert!(c.ok, "{c:?}");
    let d = c.data.as_ref().unwrap();
    assert_eq!(d["input"], "cdp", "{d}");
    assert_eq!(d["value_after"], "first", "{d}");
    assert_eq!(text(&b, &t, "state").await, "rx=first");

    // The existing content is replaced, not appended to.
    let c = act(&m, &t, "#rx", "type", json!({ "value": "second" })).await;
    assert!(c.ok, "{c:?}");
    assert_eq!(c.data.as_ref().unwrap()["value_after"], "second");
    assert_eq!(text(&b, &t, "state").await, "rx=second");

    // Empty text clears, through the same setter.
    let c = act(&m, &t, "#rx", "type", json!({ "value": "" })).await;
    assert!(c.ok, "{c:?}");
    assert_eq!(text(&b, &t, "state").await, "rx=");

    let c = act(&m, &t, "#pw", "type", json!({ "value": "hunter2" })).await;
    assert!(c.ok, "{c:?}");
    let d = c.data.as_ref().unwrap();
    assert!(d.get("value_after").is_none(), "password echoed: {d}");
    assert_eq!(d["value_length"], 7, "{d}");
    assert_eq!(text(&b, &t, "state").await, "pw=hunter2");

    let f = m
        .call(
            "browser_fill_form",
            json!({ "target_id": t, "fields": [{ "selector": "#rx", "value": "filled" }] }),
            &ctx(),
        )
        .await;
    assert!(f.ok, "{f:?}");
    assert_eq!(text(&b, &t, "state").await, "rx=filled");
    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

/// `press` sends arrow keys that move a listbox selection.
#[tokio::test(flavor = "multi_thread")]
async fn arrow_keys_move_a_listbox_selection() {
    let Some((b, t, m, stop)) = setup().await else {
        return;
    };
    for _ in 0..2 {
        let p = act(&m, &t, "#lb", "press", json!({ "value": "ArrowDown" })).await;
        assert!(p.ok, "{p:?}");
    }
    let p = act(&m, &t, "#lb", "press", json!({ "value": "ArrowUp" })).await;
    assert!(p.ok, "{p:?}");
    assert_eq!(text(&b, &t, "sel").await, "1");
    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}
