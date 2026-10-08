//! Live tests (real headless Chrome) for real pointer and keyboard input
//! through `browser_act`: coordinates, double/triple/right click, drag, wheel
//! scroll, key combos, a key with nothing focused, and clicks on SVG.
//!
//! The failures this guards (MiniWoB++ with a small model): `ctrl+a` was
//! refused, a `press` with nothing focused was an error, `el.click is not a
//! function` on SVG shapes, and a canvas could not be clicked at a point. Each
//! test asserts what the page saw, not just `ok`. Skipped when
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

/// Everything is absolutely placed in the top 460px so one viewport sees it,
/// and a tall spacer below gives the wheel something to scroll.
const PAGE: &str = r##"<!doctype html><body style="margin:0;height:3500px">
<style>
  .a { position:absolute; box-sizing:border-box; }
  #box { left:230px; top:10px; width:60px; height:40px; background:#c33; z-index:5; }
  #zone { left:230px; top:100px; width:120px; height:60px; background:#ccc; }
  #card { left:400px; top:10px; width:60px; height:40px; background:#36c; }
  #bin { left:400px; top:100px; width:120px; height:60px; background:#ccc; }
  #cover { left:10px; top:320px; width:100px; height:40px; background:#eee; }
</style>
<canvas id="cv" class="a" style="left:10px;top:10px" width="200" height="120"></canvas>
<div id="box" class="a">box</div>
<div id="zone" class="a">zone</div>
<div id="card" class="a" draggable="true">card</div>
<div id="bin" class="a">bin</div>
<input id="rng" class="a" type="range" min="0" max="100" value="0" style="left:10px;top:150px;width:300px">
<svg class="a" style="left:10px;top:190px" width="200" height="110">
  <circle id="circ" cx="40" cy="50" r="30" fill="#393"></circle>
  <path id="ring" d="M120 20 A30 30 0 1 1 119.9 20" fill="none" stroke="#933" stroke-width="6"></path>
</svg>
<svg class="a" style="left:10px;top:320px" width="100" height="40"><rect id="under" x="0" y="0" width="100" height="40" fill="#99c"></rect></svg>
<div id="cover" class="a"></div>
<textarea id="ta" class="a" style="left:10px;top:380px;width:300px;height:60px">alpha beta
gamma delta
epsilon</textarea>
<input id="inp" class="a" style="left:330px;top:380px;width:200px">
<div id="dbl" class="a" style="left:560px;top:380px;width:100px;height:40px;background:#fd8">dbl</div>
<div id="logs" class="a" style="left:560px;top:10px;width:230px;font:11px monospace"></div>
<script>
var L = {cv:'', box:'', zone:'', bin:'', svg:'', ev:'', key:''};
function put(k, v){ L[k] += v; document.getElementById('logs').textContent = JSON.stringify(L); }
window.L = L;
document.getElementById('cv').addEventListener('click', function(e){ L.cv = e.offsetX + ',' + e.offsetY; });
// A mouse-event drag, as jQuery UI does it.
var drag = null, moves = 0;
document.getElementById('box').addEventListener('mousedown', function(e){ drag = {dx: e.clientX - this.offsetLeft, dy: e.clientY - this.offsetTop}; moves = 0; L.box = 'down'; });
document.addEventListener('mousemove', function(e){ if(drag && e.buttons === 1){ var b = document.getElementById('box'); b.style.left = (e.clientX - drag.dx) + 'px'; b.style.top = (e.clientY - drag.dy) + 'px'; moves++; } });
document.addEventListener('mouseup', function(e){
  if(!drag) return; drag = null; L.box = 'down,moves=' + moves + ',up';
  var z = document.getElementById('zone').getBoundingClientRect();
  if(e.clientX >= z.left && e.clientX < z.right && e.clientY >= z.top && e.clientY < z.bottom) L.zone = 'dropped';
});
// HTML5 drag and drop.
var card = document.getElementById('card'), bin = document.getElementById('bin');
card.addEventListener('dragstart', function(e){ e.dataTransfer.setData('text/plain', 'card1'); L.bin = 'start;'; });
bin.addEventListener('dragover', function(e){ e.preventDefault(); });
bin.addEventListener('drop', function(e){ e.preventDefault(); L.bin += 'got:' + e.dataTransfer.getData('text/plain'); });
document.getElementById('circ').addEventListener('click', function(e){ L.svg += 'circ(' + e.isTrusted + ');'; });
document.getElementById('ring').addEventListener('click', function(e){ L.svg += 'ring;'; });
document.getElementById('under').addEventListener('click', function(e){ L.svg += 'under(' + e.isTrusted + ');'; });
document.getElementById('dbl').addEventListener('dblclick', function(){ L.ev += 'dbl;'; });
document.getElementById('dbl').addEventListener('contextmenu', function(e){ e.preventDefault(); L.ev += 'ctx;'; });
document.addEventListener('keydown', function(e){
  L.key += (e.ctrlKey ? 'C-' : '') + (e.metaKey ? 'M-' : '') + (e.shiftKey ? 'S-' : '') + e.key + ' ';
});
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

    async fn text(&self, js: &str) -> String {
        self.js(js).await.as_str().unwrap_or_default().to_string()
    }

    async fn log(&self, k: &str) -> String {
        self.text(&format!("window.L[{k:?}]")).await
    }

    async fn finish(self) {
        let _ = self.stop.send(());
        let _ = self.b.disconnect(1, true).await;
    }
}

fn err(e: &Envelope) -> String {
    e.error.as_ref().expect("error").message.clone()
}

/// Canvas tasks give coordinates inside the canvas: x and y are offsets from
/// its corner, as numbers or quoted numbers, and a point off screen is an
/// error rather than a click on nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_click_at_coordinates_lands_at_that_offset() {
    let Some(e) = Env::new().await else {
        return;
    };
    let d = e
        .ok(json!({ "action": "click", "query": "#cv", "x": "60", "y": "115" }))
        .await;
    assert_eq!(e.log("cv").await, "60,115", "{d}");
    assert_eq!(d["input"], "cdp");
    assert_eq!(
        d["click_at"]["x"], 70.0,
        "viewport px: canvas left 10 + 60: {d}"
    );
    assert_eq!(d["click_at"]["y"], 125.0, "{d}");

    // Without a target the point is a viewport point.
    let d = e.ok(json!({ "action": "click", "x": 30, "y": 40 })).await;
    assert_eq!(e.log("cv").await, "20,30", "{d}");
    assert_eq!(d["click_at"]["x"], 30.0);

    // Off the viewport, and only one coordinate without a target: errors.
    let far = e.act(json!({ "action": "click", "x": 5000, "y": 5 })).await;
    assert!(!far.ok);
    assert!(err(&far).contains("outside"), "{}", err(&far));
    let half = e.act(json!({ "action": "click", "x": 5 })).await;
    assert!(!half.ok);
    assert!(err(&half).contains("x and y"), "{}", err(&half));
    let word = e
        .act(json!({ "action": "click", "query": "#cv", "x": "left" }))
        .await;
    assert!(!word.ok);
    assert!(err(&word).contains("'x'"), "{}", err(&word));
    e.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn double_triple_and_right_clicks_are_real() {
    let Some(e) = Env::new().await else {
        return;
    };
    e.ok(json!({ "action": "double_click", "query": "#dbl" }))
        .await;
    e.ok(json!({ "action": "right_click", "query": "#dbl" }))
        .await;
    assert_eq!(e.log("ev").await, "dbl;ctx;");

    // Spellings a model reaches for.
    e.ok(json!({ "action": "dblclick", "query": "#dbl" })).await;
    e.ok(json!({ "action": "context-click", "query": "#dbl" }))
        .await;
    assert_eq!(e.log("ev").await, "dbl;ctx;dbl;ctx;");

    // A triple click on the middle line of a textarea selects that line.
    e.ok(json!({ "action": "triple-click", "query": "#ta", "x": 40, "y": 27 }))
        .await;
    let picked = e
        .text("(function(){var t=document.getElementById('ta');return t.value.substring(t.selectionStart,t.selectionEnd);})()")
        .await;
    assert_eq!(picked.trim(), "gamma delta", "{picked:?}");
    e.finish().await;
}

/// A drag made of mouse events, as jQuery UI and sliders listen for them.
#[tokio::test(flavor = "multi_thread")]
async fn a_drag_moves_a_mouse_event_draggable_onto_its_zone() {
    let Some(e) = Env::new().await else {
        return;
    };
    let d = e
        .ok(json!({ "action": "drag", "query": "#box", "to_query": "#zone" }))
        .await;
    assert_eq!(e.log("zone").await, "dropped", "{d}");
    let log = e.log("box").await;
    let moves: u32 = log
        .split("moves=")
        .nth(1)
        .and_then(|s| s.split(',').next())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    assert!(
        log.starts_with("down") && log.ends_with("up") && moves >= 10,
        "{log}"
    );
    assert_eq!(d["from"]["x"], 260.0, "{d}");
    assert_eq!(d["to"]["x"], 290.0, "{d}");
    assert_eq!(d["html5_drag"], false);

    // Source and destination as viewport points; then relative to the start.
    e.ok(json!({ "action": "drag", "x": 290, "y": 130, "to_x": 100, "to_y": 20 }))
        .await;
    let left = e.js("document.getElementById('box').offsetLeft").await;
    assert!((left.as_f64().unwrap() - 70.0).abs() <= 1.0, "{left}");
    e.ok(json!({ "action": "drag", "query": "#box", "dx": 0, "dy": 100 }))
        .await;
    let top = e.js("document.getElementById('box').offsetTop").await;
    assert!((top.as_f64().unwrap() - 100.0).abs() <= 1.0, "{top}");

    let none = e.act(json!({ "action": "drag", "query": "#box" })).await;
    assert!(!none.ok);
    assert!(err(&none).contains("destination"), "{}", err(&none));
    e.finish().await;
}

/// `draggable=true` elements get `dragstart` and `drop` only from drag events.
#[tokio::test(flavor = "multi_thread")]
async fn a_drag_drops_an_html5_draggable() {
    let Some(e) = Env::new().await else {
        return;
    };
    let d = e
        .ok(json!({ "action": "drag_and_drop", "query": "#card", "to_query": "#bin" }))
        .await;
    assert_eq!(e.log("bin").await, "start;got:card1", "{d}");
    assert_eq!(d["html5_drag"], true, "{d}");
    e.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_drag_slides_a_range_input() {
    let Some(e) = Env::new().await else {
        return;
    };
    // The thumb sits at the left edge: grab it there, release mid-track.
    e.ok(json!({ "action": "drag", "query": "#rng", "x": 8, "to_query": "#rng", "to_x": 150 }))
        .await;
    let v = e.js("+document.getElementById('rng').value").await;
    assert!((v.as_f64().unwrap() - 50.0).abs() <= 4.0, "{v}");
    e.finish().await;
}

/// SVG elements have no `click()`: a real click lands on the shape, a shape
/// the box centre misses is found, and a covered one gets a dispatched event
/// sequence instead of a `TypeError`.
#[tokio::test(flavor = "multi_thread")]
async fn svg_shapes_can_be_clicked() {
    let Some(e) = Env::new().await else {
        return;
    };
    let d = e.ok(json!({ "action": "click", "query": "#circ" })).await;
    assert_eq!(d["input"], "cdp", "{d}");
    // The ring's box centre is empty: another point on the stroke is used.
    let d = e.ok(json!({ "action": "click", "query": "#ring" })).await;
    assert_eq!(d["input"], "cdp", "{d}");
    // A div covers #under, so the click cannot reach it with a pointer.
    let d = e.ok(json!({ "action": "click", "query": "#under" })).await;
    assert_eq!(d["input"], "synthetic", "{d}");
    assert_eq!(e.log("svg").await, "circ(true);ring;under(false);");
    e.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn key_combos_select_copy_and_paste() {
    let Some(e) = Env::new().await else {
        return;
    };
    e.ok(json!({ "action": "focus", "query": "#ta" })).await;
    let d = e.ok(json!({ "action": "press", "value": "ctrl+a" })).await;
    assert_eq!(d["key"], "a", "{d}");
    assert_eq!(d["modifiers"], json!(["Control"]), "{d}");
    // Select-all took effect, so typing replaces the whole text.
    e.ok(json!({ "action": "press", "value": "q" })).await;
    assert_eq!(e.text("document.getElementById('ta').value").await, "q");
    assert!(
        e.log("key").await.contains("C-a "),
        "{}",
        e.log("key").await
    );

    // Spellings: Control+A, cmd+a; then copy from the textarea, paste into
    // the input.
    e.js("document.getElementById('ta').value = 'hello world'; true")
        .await;
    e.ok(json!({ "action": "press", "query": "#ta", "value": "Control+A" }))
        .await;
    e.ok(json!({ "action": "press", "value": "ctrl+c" })).await;
    e.ok(json!({ "action": "press", "query": "#inp", "value": "ctrl+v" }))
        .await;
    assert_eq!(
        e.text("document.getElementById('inp').value").await,
        "hello world"
    );

    // Cut and undo.
    e.ok(json!({ "action": "press", "query": "#inp", "value": "ctrl+a" }))
        .await;
    e.ok(json!({ "action": "press", "value": "ctrl+x" })).await;
    assert_eq!(e.text("document.getElementById('inp').value").await, "");
    e.ok(json!({ "action": "press", "value": "ctrl+z" })).await;
    assert_eq!(
        e.text("document.getElementById('inp').value").await,
        "hello world"
    );

    // Shift+Arrow extends a selection.
    e.js("(function(){var t=document.getElementById('ta');t.value='abcdef';t.focus();t.setSelectionRange(0,0);return true;})()")
        .await;
    e.ok(json!({ "action": "press", "value": "Shift+ArrowRight" }))
        .await;
    e.ok(json!({ "action": "key", "value": "shift-right" }))
        .await;
    assert_eq!(e.js("document.getElementById('ta').selectionEnd").await, 2);

    let bad = e
        .act(json!({ "action": "press", "value": "ctrl+nope" }))
        .await;
    assert!(!bad.ok);
    assert!(
        err(&bad).contains("Escape") && err(&bad).contains("ctrl+a"),
        "{}",
        err(&bad)
    );
    e.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_key_with_nothing_focused_goes_to_the_page() {
    let Some(e) = Env::new().await else {
        return;
    };
    let d = e.ok(json!({ "action": "press", "value": "Escape" })).await;
    assert_eq!(d["target"]["tag"], "body", "{d}");
    e.ok(json!({ "action": "press", "value": "F5" })).await;
    e.ok(json!({ "action": "press", "value": "Shift+a" })).await;
    assert_eq!(e.log("key").await, "Escape F5 S-Shift S-A ");
    e.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn scroll_turns_the_wheel() {
    let Some(e) = Env::new().await else {
        return;
    };
    let vh = e.js("innerHeight").await.as_f64().unwrap();
    let d = e.ok(json!({ "action": "scroll" })).await;
    let y = e.js("scrollY").await.as_f64().unwrap();
    assert!((y - vh).abs() <= 2.0, "one viewport down: {y} vs {vh}: {d}");
    assert_eq!(d["moved"], true, "{d}");

    e.ok(json!({ "action": "scroll", "dy": -100 })).await;
    let y2 = e.js("scrollY").await.as_f64().unwrap();
    assert!((y - y2 - 100.0).abs() <= 2.0, "{y} -> {y2}");

    // The model's own spelling: a query of body.
    e.ok(json!({ "action": "scroll", "query": "body", "value": "top" }))
        .await;
    assert_eq!(e.js("scrollY").await, 0);
    e.ok(json!({ "action": "scroll", "value": "bottom" })).await;
    let max = e
        .js("document.documentElement.scrollHeight - innerHeight")
        .await;
    assert_eq!(e.js("scrollY").await, max);
    let end = e
        .act(json!({ "action": "scroll", "value": "sideways" }))
        .await;
    assert!(!end.ok && err(&end).contains("sideways"), "{end:?}");
    e.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_action_lists_the_valid_ones() {
    let Some(e) = Env::new().await else {
        return;
    };
    let bad = e
        .act(json!({ "action": "teleport", "query": "#box" }))
        .await;
    assert!(!bad.ok);
    let m = err(&bad);
    assert!(
        m.contains("'teleport'") && m.contains("double_click") && m.contains("drag"),
        "{m}"
    );
    e.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn wait_with_only_a_duration_sleeps() {
    let Some(e) = Env::new().await else {
        return;
    };
    let t0 = std::time::Instant::now();
    let w =
        e.m.call(
            "browser_wait",
            json!({ "target_id": e.t, "timeout_ms": 300 }),
            &ctx(),
        )
        .await;
    assert!(w.ok, "{w:?}");
    assert_eq!(w.data.unwrap()["waited_ms"], 300);
    assert!(t0.elapsed().as_millis() >= 290);
    e.finish().await;
}

/// The showcase cursor must keep working for the new pointer actions.
#[tokio::test(flavor = "multi_thread")]
async fn pointer_actions_work_under_the_showcase() {
    let Some(e) = Env::new().await else {
        return;
    };
    e.b.showcase(&e.t, Some(mcp_browser::showcase::ShowcaseConfig::snappy()))
        .await
        .expect("showcase");
    e.ok(json!({ "action": "double_click", "query": "#dbl" }))
        .await;
    e.ok(json!({ "action": "drag", "query": "#box", "to_query": "#zone" }))
        .await;
    assert_eq!(e.log("ev").await, "dbl;");
    assert_eq!(e.log("zone").await, "dropped");
    e.finish().await;
}
