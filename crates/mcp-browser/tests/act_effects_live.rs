//! Live tests (real headless Chrome) for the pointer primitives that compose
//! gestures (`mouse_move`, `mouse_down`, `mouse_up`, `hold_ms`), for `hit`
//! on every coordinate action, and for the `effects` object that tells a model
//! what its action did without a fresh snapshot.
//!
//! The failures this guards (MiniWoB++ with a small model): drags and holds
//! the page implements itself could not be built from a click, a click at a
//! point said ok whatever was under it, and after each action the model took
//! a snapshot just to learn that a popup had opened or a form had appeared.
//! Each test asserts what the page saw, not just `ok`. Skipped when
//! `AGENTCTL_SKIP_LIVE` is set or no Chrome binary is found.

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

/// Everything sits in the top 460px so one viewport sees it. The page logs
/// what it heard into `window.L`; the modal and the reply form start hidden.
const PAGE: &str = r##"<!doctype html><body style="margin:0;height:1200px">
<style>
  .a { position:absolute; box-sizing:border-box; }
  #hov { left:10px; top:10px; width:120px; height:60px; background:#cde; }
  #trk { left:10px; top:100px; width:400px; height:40px; background:#eee; }
  #knob { left:10px; top:100px; width:30px; height:40px; background:#c33; }
  #hold { left:10px; top:160px; width:100px; height:40px; }
  #cv { left:150px; top:160px; background:#fe9; }
  #open { left:10px; top:220px; width:120px; height:30px; }
  #nav { left:10px; top:265px; }
  #pop { left:10px; top:300px; width:100px; height:30px; }
  #foc { left:10px; top:340px; width:100px; height:30px; }
  #name { left:150px; top:340px; width:150px; }
  #alert { left:10px; top:385px; width:100px; height:30px; }
  #reply { left:150px; top:385px; width:100px; height:30px; }
  #form { left:300px; top:385px; width:300px; display:none; }
  #m { position:fixed; left:400px; top:200px; width:300px; height:120px; background:#fff; border:2px solid #333; z-index:1000; display:none; }
</style>
<div id="hov" class="a">hover zone</div>
<div id="trk" class="a"></div>
<div id="knob" class="a"></div>
<button id="hold" class="a">Hold me</button>
<canvas id="cv" class="a" width="200" height="100"></canvas>
<button id="open" class="a">Open modal</button>
<a id="nav" class="a" href="/two">Next page</a>
<button id="pop" class="a">Popup</button>
<button id="foc" class="a">Focus name</button>
<input id="name" class="a" placeholder="Name">
<button id="alert" class="a">Alert</button>
<button id="reply" class="a">Reply</button>
<div id="form" class="a"><textarea id="body" placeholder="Your reply"></textarea><button id="send">Send</button></div>
<div id="m" role="dialog" aria-modal="true"><h2>Session expired</h2><button id="dismiss">Dismiss</button></div>
<script>
document.title = location.pathname;
var L = {over:'', down:'', drag:'', btn:'', hold:'', key:'', cv:''};
window.L = L;
var hov = document.getElementById('hov');
hov.addEventListener('mouseover', function(e){ L.over += 'over@' + e.clientX + ',' + e.clientY + ';'; });
hov.addEventListener('mousedown', function(e){ L.down += 'b' + e.button + ';'; });
// A drag the page implements itself: mousedown on the knob, moves with the
// button held, mouseup anywhere.
var knob = document.getElementById('knob'), drag = null;
knob.addEventListener('mousedown', function(){ drag = {moves: 0, held: true}; });
document.addEventListener('mousemove', function(e){
  if(!drag) return;
  drag.moves++; if(e.buttons !== 1) drag.held = false;
  knob.style.left = (e.clientX - 15) + 'px';
});
document.addEventListener('mouseup', function(e){
  if(!drag) return;
  L.drag = 'moves=' + drag.moves + ',held=' + drag.held + ',up@' + e.clientX + ',' + e.clientY;
  drag = null;
});
// A press the page times.
var t0 = 0, k0 = 0;
document.getElementById('hold').addEventListener('mousedown', function(){ t0 = performance.now(); });
document.getElementById('hold').addEventListener('mouseup', function(){ L.hold = String(Math.round(performance.now() - t0)); });
document.addEventListener('keydown', function(e){ if(!e.repeat) k0 = performance.now(); });
document.addEventListener('keyup', function(){ L.key = String(Math.round(performance.now() - k0)); });
document.getElementById('cv').addEventListener('click', function(e){ L.cv = e.offsetX + ',' + e.offsetY; });
document.getElementById('open').onclick = function(){ setTimeout(function(){ document.getElementById('m').style.display = 'block'; }, 20); };
document.getElementById('dismiss').onclick = function(){ document.getElementById('m').style.display = 'none'; };
document.getElementById('pop').onclick = function(){ window.open('/popup?x=1', '_blank'); };
document.getElementById('foc').onclick = function(){ document.getElementById('name').focus(); };
document.getElementById('alert').onclick = function(){ alert('hello'); };
document.getElementById('reply').onclick = function(){ document.getElementById('form').style.display = 'block'; };
</script></body>"##;

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

struct Env {
    b: Arc<CdpBackend>,
    t: String,
    m: BrowserModule,
    stop: tokio::sync::oneshot::Sender<()>,
}

impl Env {
    async fn new() -> Option<Env> {
        let (b, t) = tab().await?;
        let (base, stop) = serve().await;
        b.navigate(&t, "goto", Some(&format!("{base}/")))
            .await
            .expect("goto");
        let m = BrowserModule::new(b.clone());
        Some(Env { b, t, m, stop })
    }

    /// `browser_act` with `args` merged over the tab.
    async fn act(&self, mut args: Value) -> Envelope {
        args["target_id"] = json!(self.t);
        self.m.call("browser_act", args, &ctx()).await
    }

    async fn ok(&self, args: Value) -> Value {
        let e = self.act(args.clone()).await;
        assert!(e.ok, "{args} -> {e:?}");
        e.data.expect("data")
    }

    async fn js(&self, js: &str) -> Value {
        self.b.eval(&self.t, js).await.expect("eval")["result"].clone()
    }

    async fn log(&self, k: &str) -> String {
        self.js(&format!("window.L[{k:?}]"))
            .await
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    async fn finish(self) {
        let _ = self.stop.send(());
        let _ = self.b.disconnect(1, true).await;
    }
}

fn err(e: &Envelope) -> String {
    e.error.as_ref().expect("error").message.clone()
}

/// The strings of `effects.<key>`, empty when the key is absent.
fn items(d: &Value, key: &str) -> Vec<String> {
    d["effects"][key]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn mouse_move_hovers_without_clicking() {
    let Some(e) = Env::new().await else {
        return;
    };
    let d = e
        .ok(json!({ "action": "mouse_move", "x": 60, "y": 40 }))
        .await;
    assert_eq!(e.log("over").await, "over@60,40;", "{d}");
    assert_eq!(e.log("down").await, "", "a move presses nothing: {d}");
    assert_eq!(d["at"]["x"], 60.0, "{d}");
    assert_eq!(d["hit"]["id"], "hov", "{d}");

    // Off the zone and back with `hover` and a point: the same real move.
    e.ok(json!({ "action": "mouse_move", "x": 300, "y": 420 }))
        .await;
    let d = e.ok(json!({ "action": "hover", "x": 70, "y": 45 })).await;
    assert_eq!(e.log("over").await, "over@60,40;over@70,45;", "{d}");

    // An element target, and the spellings a model writes.
    e.ok(json!({ "action": "mouse_move", "x": 300, "y": 420 }))
        .await;
    e.ok(json!({ "action": "mousemove", "query": "#hov" }))
        .await;
    assert!(
        e.log("over").await.ends_with("over@70,40;"),
        "centre of the zone"
    );
    e.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mouse_down_and_up_compose_a_drag_the_page_tracks() {
    let Some(e) = Env::new().await else {
        return;
    };
    let d = e
        .ok(json!({ "action": "mouse_down", "query": "#knob" }))
        .await;
    assert_eq!(d["held"], "left", "{d}");
    // Each call is its own connection; the held button carries over.
    let d = e
        .ok(json!({ "action": "mouse_move", "x": 220, "y": 120 }))
        .await;
    assert_eq!(d["hit"]["id"], "trk", "{d}");
    let d = e.ok(json!({ "action": "mouse_up" })).await;
    assert_eq!(d["released"], "left", "{d}");
    let drag = e.log("drag").await;
    assert!(
        drag.contains("held=true") && drag.ends_with("up@220,120"),
        "moves carried the held button and the release was at the end: {drag}"
    );
    let moves: u32 = drag
        .strip_prefix("moves=")
        .and_then(|r| r.split(',').next())
        .and_then(|n| n.parse().ok())
        .expect(&drag);
    assert!(moves >= 3, "a drag arrives in steps: {drag}");
    let left = e.js("document.getElementById('knob').style.left").await;
    assert_eq!(left, "205px", "the page moved the knob: {left}");

    // A later move is a hover again: the button is no longer held.
    e.ok(json!({ "action": "mouse_move", "x": 60, "y": 40 }))
        .await;
    assert_eq!(e.log("drag").await, drag);

    // Other buttons, and the spelling.
    e.ok(json!({ "action": "mousedown", "query": "#hov", "button": "right" }))
        .await;
    e.ok(json!({ "action": "mouseup", "button": "right" }))
        .await;
    assert_eq!(e.log("down").await, "b2;");
    let bad = e
        .act(json!({ "action": "mouse_down", "query": "#hov", "button": "fourth" }))
        .await;
    assert!(!bad.ok && err(&bad).contains("left, right or middle"));
    e.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_click_or_key_can_be_held_for_a_time() {
    let Some(e) = Env::new().await else {
        return;
    };
    let d = e
        .ok(json!({ "action": "click", "query": "#hold", "hold_ms": 400 }))
        .await;
    assert_eq!(d["held_ms"], 400, "{d}");
    let ms: u32 = e.log("hold").await.parse().expect("press measured");
    assert!((380..1500).contains(&ms), "held about 400 ms: {ms}");

    // `long_press` is a held click, 800 ms unless told otherwise.
    e.ok(json!({ "action": "long_press", "query": "#hold" }))
        .await;
    let ms: u32 = e.log("hold").await.parse().expect("press measured");
    assert!((780..2000).contains(&ms), "held about 800 ms: {ms}");

    // A key down and up that far apart.
    e.ok(json!({ "action": "press", "value": "a", "hold_ms": 300 }))
        .await;
    let ms: u32 = e.log("key").await.parse().expect("key measured");
    assert!((280..1500).contains(&ms), "key held about 300 ms: {ms}");

    // Too long, negative, and on an action that cannot be held.
    let long = e
        .act(json!({ "action": "click", "query": "#hold", "hold_ms": 10001 }))
        .await;
    assert!(!long.ok && err(&long).contains("10000"), "{}", err(&long));
    let neg = e
        .act(json!({ "action": "click", "query": "#hold", "hold_ms": -5 }))
        .await;
    assert!(!neg.ok);
    let typed = e
        .act(json!({ "action": "type", "query": "#name", "value": "x", "hold_ms": 100 }))
        .await;
    assert!(
        !typed.ok && err(&typed).contains("hold_ms applies"),
        "{}",
        err(&typed)
    );
    e.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_coordinate_action_says_what_it_hit() {
    let Some(e) = Env::new().await else {
        return;
    };
    // A canvas is clicked at its own pixels; the hit says it is a canvas.
    let d = e
        .ok(json!({ "action": "click", "query": "#cv", "x": 50, "y": 40 }))
        .await;
    assert_eq!(e.log("cv").await, "50,40", "{d}");
    assert_eq!(d["hit"]["tag"], "canvas", "{d}");
    assert_eq!(d["hit"]["canvas"], true, "{d}");
    assert_eq!(d["hit"]["id"], "cv", "{d}");

    // A plain element: tag, id and its text.
    let d = e.ok(json!({ "action": "click", "x": 60, "y": 40 })).await;
    assert_eq!(d["hit"]["tag"], "div", "{d}");
    assert_eq!(d["hit"]["canvas"], false, "{d}");
    assert_eq!(d["hit"]["text"], "hover zone", "{d}");

    // Every pointer action reports it; the empty page is a hit as well.
    let d = e
        .ok(json!({ "action": "right_click", "x": 600, "y": 440 }))
        .await;
    assert_eq!(d["hit"]["tag"], "body", "{d}");
    let d = e
        .ok(json!({ "action": "scroll", "x": 60, "y": 40, "dy": 0 }))
        .await;
    assert_eq!(d["hit"]["id"], "hov", "{d}");

    // A point off the screen is an error that names the viewport.
    let far = e
        .act(json!({ "action": "mouse_move", "x": 5000, "y": 5 }))
        .await;
    assert!(!far.ok);
    let m = err(&far);
    assert!(m.contains("outside") && m.contains("viewport"), "{m}");
    assert!(
        m.chars().filter(char::is_ascii_digit).count() >= 6 && m.contains('x'),
        "the message gives the viewport size: {m}"
    );
    let far = e
        .act(json!({ "action": "mouse_down", "x": 5, "y": 90000 }))
        .await;
    assert!(!far.ok && err(&far).contains("outside"));
    e.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn effects_report_a_modal_that_appears_and_goes() {
    let Some(e) = Env::new().await else {
        return;
    };
    let d = e.ok(json!({ "action": "click", "query": "#open" })).await;
    let appeared = items(&d, "appeared");
    assert!(
        appeared[0].starts_with("dialog \"Session expired\"") && appeared[0].contains("ref="),
        "the dialog comes first, with a ref: {appeared:?}"
    );
    assert!(
        appeared.iter().any(|a| a.starts_with("button \"Dismiss\"")),
        "{appeared:?}"
    );
    assert!(
        d["effects"].get("url").is_none() && d["effects"].get("disappeared").is_none(),
        "{d}"
    );
    let size = d["effects"].to_string().len();
    assert!(size < 400, "the report stays short ({size} bytes): {d}");

    // Its ref works, and closing it is reported as the dialog going.
    let r = appeared[0].split("ref=").nth(1).expect("ref").to_string();
    let d = e
        .ok(json!({ "action": "click", "query": "#dismiss" }))
        .await;
    let gone = items(&d, "disappeared");
    assert!(
        gone[0].starts_with("dialog \"Session expired\""),
        "{gone:?}"
    );
    assert!(items(&d, "appeared").is_empty());
    let found = e
        .js(&format!(
            "document.evaluate({r:?}, document, null, 9, null).singleNodeValue !== null"
        ))
        .await;
    assert_eq!(found, true, "ref {r} names the dialog");

    // An action that changes nothing carries no effects at all.
    let d = e.ok(json!({ "action": "click", "x": 600, "y": 440 })).await;
    assert!(d.get("effects").is_none(), "{d}");
    e.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn effects_report_a_form_revealed_by_a_click() {
    let Some(e) = Env::new().await else {
        return;
    };
    let d = e.ok(json!({ "action": "click", "query": "#reply" })).await;
    let appeared = items(&d, "appeared");
    assert!(
        appeared
            .iter()
            .any(|a| a.starts_with("textbox \"Your reply\"")),
        "{appeared:?}"
    );
    assert!(
        appeared.iter().any(|a| a.starts_with("button \"Send\"")),
        "{appeared:?}"
    );
    e.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn effects_report_a_url_change_and_nothing_else_for_a_navigation() {
    let Some(e) = Env::new().await else {
        return;
    };
    let d = e.ok(json!({ "action": "click", "query": "#nav" })).await;
    let url = d["effects"]["url"].as_str().unwrap_or_default();
    assert!(url.ends_with("/two"), "{d}");
    assert_eq!(d["effects"]["title"], "/two", "{d}");
    assert!(
        d["effects"].get("appeared").is_none() && d["effects"].get("disappeared").is_none(),
        "a new page is not a list of what appeared: {d}"
    );
    e.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn effects_report_a_popup_as_a_new_tab() {
    let Some(e) = Env::new().await else {
        return;
    };
    let d = e.ok(json!({ "action": "click", "query": "#pop" })).await;
    let tab = &d["effects"]["new_tab"];
    let id = tab["target_id"].as_str().unwrap_or_default();
    assert!(!id.is_empty() && id != e.t, "{d}");
    assert!(
        tab["url"]
            .as_str()
            .unwrap_or_default()
            .contains("/popup?x=1"),
        "the url is the popup's, not about:blank: {d}"
    );
    e.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn effects_report_where_the_focus_went() {
    let Some(e) = Env::new().await else {
        return;
    };
    let d = e.ok(json!({ "action": "click", "query": "#foc" })).await;
    let focus = d["effects"]["focus"].as_str().unwrap_or_default();
    assert!(focus.starts_with("textbox \"Name\""), "{d}");
    // Focus that does not move is not reported again.
    let d = e
        .ok(json!({ "action": "type", "query": "#name", "value": "Ada" }))
        .await;
    assert!(d.get("effects").is_none(), "{d}");
    // Clicking another control moves it.
    let d = e.ok(json!({ "action": "click", "query": "#hold" })).await;
    assert!(
        d["effects"]["focus"]
            .as_str()
            .unwrap_or_default()
            .starts_with("button \"Hold me\""),
        "{d}"
    );
    e.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn effects_report_a_javascript_dialog() {
    let Some(e) = Env::new().await else {
        return;
    };
    let d = e.ok(json!({ "action": "click", "query": "#alert" })).await;
    let dlg = &d["effects"]["dialog"];
    assert_eq!(dlg["type"], "alert", "{d}");
    assert_eq!(dlg["message"], "hello", "{d}");
    assert_eq!(dlg["answered"], "dismissed", "{d}");
    e.finish().await;
}
