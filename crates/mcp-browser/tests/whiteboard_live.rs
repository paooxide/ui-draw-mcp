//! Live tests (real headless Chrome) for `browser_act` as a drawing tool
//! drives it: modifier keys held through pointer actions, a drag's pacing,
//! button and waypoint path, a polygon from a batch of clicks, an SVG vertex
//! drag, and canvas coordinates given in bitmap pixels on a CSS-scaled
//! canvas. The page is `docs/fixtures/whiteboard.html`, which records what
//! it saw; every test asserts that, not just `ok`. Skipped when
//! `AGENTCTL_SKIP_LIVE` is set or no Chrome binary is found.

use std::path::PathBuf;
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

fn fixture() -> String {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates
    path.pop(); // workspace root
    path.push("docs/fixtures/whiteboard.html");
    std::fs::read_to_string(&path).expect("docs/fixtures/whiteboard.html")
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

async fn serve(page: String) -> (String, tokio::sync::oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
    let page = Arc::new(page);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut rx => break,
                res = listener.accept() => {
                    let Ok((mut stream, _)) = res else { continue };
                    let page = page.clone();
                    tokio::spawn(async move {
                        let mut buf = [0u8; 2048];
                        let _ = stream.read(&mut buf).await;
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            page.len(),
                            page
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
        let (base, stop) = serve(fixture()).await;
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

    /// A page value, read back through JSON so arrays and objects arrive whole.
    async fn json(&self, js: &str) -> Value {
        let s = self
            .b
            .eval(&self.t, &format!("JSON.stringify({js})"))
            .await
            .expect("eval")["result"]
            .clone();
        let s = s.as_str().unwrap_or("null");
        serde_json::from_str(s).unwrap_or(Value::Null)
    }

    /// The canvas's recorded events, each `{type, x, y, buttons, shift, ...}`.
    async fn events(&self) -> Vec<Value> {
        self.json("window.__events")
            .await
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    async fn clear(&self) {
        let _ = self
            .b
            .eval(&self.t, "window.__events.length = 0; true")
            .await;
    }

    async fn finish(self) {
        let _ = self.stop.send(());
        let _ = self.b.disconnect(1, true).await;
    }
}

fn err(e: &Envelope) -> String {
    e.error.as_ref().expect("error").message.clone()
}

fn of_type<'a>(events: &'a [Value], kind: &str) -> Vec<&'a Value> {
    events.iter().filter(|e| e["type"] == kind).collect()
}

fn near(v: &Value, want: f64) -> bool {
    v.as_f64().is_some_and(|x| (x - want).abs() <= 1.0)
}

/// A shift-drag: every pointer event carries shiftKey, and the Shift key
/// itself goes down before the first and up after the last, as a tool that
/// constrains a shape while Shift is held needs.
#[tokio::test(flavor = "multi_thread")]
async fn a_shift_drag_holds_shift_through_every_event() {
    let Some(e) = Env::new().await else {
        return;
    };
    let d = e
        .ok(json!({
            "action": "drag", "query": "#board", "x": 20, "y": 20, "dx": 200, "dy": 100,
            "modifiers": ["shift"]
        }))
        .await;
    assert_eq!(d["modifiers"], json!(["Shift"]), "{d}");
    let ev = e.events().await;
    assert_eq!(
        ev.first().map(|v| &v["type"]),
        Some(&json!("keydown")),
        "{ev:?}"
    );
    assert_eq!(ev.first().map(|v| &v["key"]), Some(&json!("Shift")));
    assert_eq!(
        ev.last().map(|v| &v["type"]),
        Some(&json!("keyup")),
        "{ev:?}"
    );
    let moves: Vec<&Value> = of_type(&ev, "pointermove")
        .into_iter()
        .filter(|m| m["buttons"] == 1)
        .collect();
    assert!(moves.len() >= 10, "{} held moves: {ev:?}", moves.len());
    assert!(moves.iter().all(|m| m["shift"] == true), "{ev:?}");
    assert!(of_type(&ev, "pointerdown")
        .iter()
        .all(|m| m["shift"] == true && m["ctrl"] == false));
    assert!(of_type(&ev, "pointerup").iter().all(|m| m["shift"] == true));

    // A click with a modifier, and one without, on a ref: the plain click
    // reports no modifiers and the page sees none.
    e.clear().await;
    let d = e
        .ok(json!({ "action": "click", "query": "#board", "x": 10, "y": 10, "modifiers": ["ctrl", "alt"] }))
        .await;
    assert_eq!(d["modifiers"], json!(["Control", "Alt"]), "{d}");
    let ev = e.events().await;
    let down = of_type(&ev, "pointerdown");
    assert_eq!(down.len(), 1, "{ev:?}");
    assert!(down[0]["ctrl"] == true && down[0]["alt"] == true && down[0]["shift"] == false);
    e.clear().await;
    let d = e
        .ok(json!({ "action": "click", "query": "#board", "x": 10, "y": 10 }))
        .await;
    assert!(d.get("modifiers").is_none(), "{d}");
    let ev = e.events().await;
    assert!(of_type(&ev, "keydown").is_empty(), "{ev:?}");
    assert!(of_type(&ev, "pointerdown")
        .iter()
        .all(|m| m["ctrl"] == false && m["shift"] == false));

    // Not a modifier, or a modifier on a non-pointer action: refused.
    let bad = e
        .act(json!({ "action": "click", "query": "#board", "modifiers": ["hyper"] }))
        .await;
    assert!(!bad.ok);
    assert!(err(&bad).contains("hyper"), "{}", err(&bad));
    let bad = e
        .act(json!({ "action": "focus", "query": "#board", "modifiers": ["shift"] }))
        .await;
    assert!(!bad.ok);
    assert!(err(&bad).contains("pointer"), "{}", err(&bad));
    e.finish().await;
}

/// A drag with a `path` is a brush stroke: it presses at the start, passes
/// through every waypoint in order and releases at the last. The canvas is
/// the `to_query` frame, so the waypoints are its bitmap pixels.
#[tokio::test(flavor = "multi_thread")]
async fn a_path_drag_passes_through_every_waypoint_in_order() {
    let Some(e) = Env::new().await else {
        return;
    };
    let d = e
        .ok(json!({
            "action": "drag", "query": "#board", "x": 10, "y": 10, "to_query": "#board",
            "path": [{"x": 100, "y": 10}, {"x": 100, "y": 100}, {"x": 10, "y": 100}]
        }))
        .await;
    assert_eq!(d["waypoints"], 3, "{d}");
    assert_eq!(d["moves"], 12, "{d}");
    assert_eq!(d["from"]["pixel"], json!({"x": 10.0, "y": 10.0}), "{d}");
    assert_eq!(d["to"]["pixel"], json!({"x": 10.0, "y": 100.0}), "{d}");
    let ev = e.events().await;
    let down = of_type(&ev, "pointerdown");
    assert_eq!(down.len(), 1, "{ev:?}");
    assert!(
        near(&down[0]["x"], 10.0) && near(&down[0]["y"], 10.0),
        "{ev:?}"
    );
    let moves: Vec<(f64, f64)> = of_type(&ev, "pointermove")
        .iter()
        .filter(|m| m["buttons"] == 1)
        .map(|m| (m["x"].as_f64().unwrap(), m["y"].as_f64().unwrap()))
        .collect();
    assert_eq!(moves.len(), 12, "{moves:?}");
    let mut at = 0;
    for want in [(100.0, 10.0), (100.0, 100.0), (10.0, 100.0)] {
        let i = moves[at..]
            .iter()
            .position(|m| (m.0 - want.0).abs() <= 1.0 && (m.1 - want.1).abs() <= 1.0)
            .unwrap_or_else(|| panic!("waypoint {want:?} not reached in order: {moves:?}"));
        at += i + 1;
    }
    assert_eq!(moves.last(), Some(&(10.0, 100.0)), "{moves:?}");
    // Between the corners the stroke stays on the square's sides.
    assert!(
        moves.iter().all(|m| (m.1 - 10.0).abs() <= 1.0
            || (m.0 - 100.0).abs() <= 1.0
            || (m.1 - 100.0).abs() <= 1.0),
        "{moves:?}"
    );
    let up = of_type(&ev, "pointerup");
    assert_eq!(up.len(), 1);
    assert!(
        near(&up[0]["x"], 10.0) && near(&up[0]["y"], 100.0),
        "{ev:?}"
    );
    let strokes = e.json("window.__strokes").await;
    assert_eq!(strokes.as_array().map(Vec::len), Some(1), "{strokes}");

    // Points may also be [x, y] pairs; a point off screen is refused before
    // anything is pressed; path and dx/dy together are refused.
    e.clear().await;
    let d = e
        .ok(json!({
            "action": "drag", "query": "#board", "x": 150, "y": 150, "to_query": "#board",
            "path": [[160, 150], [160, 160]], "moves": 4
        }))
        .await;
    assert_eq!(d["moves"], 4, "{d}");
    e.clear().await;
    let far = e
        .act(json!({ "action": "drag", "query": "#board", "x": 10, "y": 10, "path": [{"x": 5000, "y": 5}] }))
        .await;
    assert!(!far.ok);
    assert!(err(&far).contains("path point 0"), "{}", err(&far));
    assert!(e.events().await.is_empty(), "nothing pressed");
    let both = e
        .act(json!({ "action": "drag", "query": "#board", "path": [{"x": 50, "y": 50}], "dx": 5 }))
        .await;
    assert!(!both.ok);
    assert!(err(&both).contains("'dx'"), "{}", err(&both));
    e.finish().await;
}

/// `moves` and `duration_ms` pace a drag: the page sees about that many
/// held moves over about that long; `button` chooses what is held. The
/// defaults are 12 moves 15 ms apart.
#[tokio::test(flavor = "multi_thread")]
async fn moves_duration_and_button_pace_a_drag() {
    let Some(e) = Env::new().await else {
        return;
    };
    let started = std::time::Instant::now();
    let d = e
        .ok(json!({
            "action": "drag", "query": "#board", "x": 20, "y": 20, "dx": 400, "dy": 200,
            "moves": 20, "duration_ms": 1000
        }))
        .await;
    let took = started.elapsed();
    assert_eq!(d["moves"], 20, "{d}");
    assert_eq!(d["duration_ms"], 1000, "{d}");
    assert!(took.as_millis() >= 900, "took {took:?}");
    let ev = e.events().await;
    let held = of_type(&ev, "pointermove")
        .iter()
        .filter(|m| m["buttons"] == 1)
        .count();
    assert!((17..=22).contains(&held), "{held} held moves: {ev:?}");
    assert!(of_type(&ev, "pointerdown")
        .iter()
        .all(|m| m["button"] == 0 && m["buttons"] == 1));

    // Defaults: 12 moves, 15 ms apart.
    e.clear().await;
    let d = e
        .ok(json!({ "action": "drag", "query": "#board", "x": 20, "y": 20, "dx": 100, "dy": 0 }))
        .await;
    assert_eq!(d["moves"], 12, "{d}");
    assert_eq!(d["duration_ms"], 180, "{d}");
    let ev = e.events().await;
    let held = of_type(&ev, "pointermove")
        .iter()
        .filter(|m| m["buttons"] == 1)
        .count();
    assert!((10..=13).contains(&held), "{held} held moves: {ev:?}");

    // A right-button drag: the page sees button 2 held.
    e.clear().await;
    let d = e
        .ok(json!({
            "action": "drag", "query": "#board", "x": 20, "y": 120, "dx": 50, "dy": 0,
            "button": "right", "moves": 5
        }))
        .await;
    assert_eq!(d["button"], "right", "{d}");
    let ev = e.events().await;
    let down = of_type(&ev, "pointerdown");
    assert_eq!(down.len(), 1, "{ev:?}");
    assert!(down[0]["button"] == 2 && down[0]["buttons"] == 2, "{ev:?}");
    assert!(of_type(&ev, "pointermove")
        .iter()
        .filter(|m| m["buttons"] != 0)
        .all(|m| m["buttons"] == 2));

    // Out of range, or on the wrong action: refused.
    let one = e
        .act(json!({ "action": "drag", "query": "#board", "dx": 5, "moves": 1 }))
        .await;
    assert!(!one.ok);
    assert!(err(&one).contains("2 to 200"), "{}", err(&one));
    let long = e
        .act(json!({ "action": "drag", "query": "#board", "dx": 5, "duration_ms": 20000 }))
        .await;
    assert!(!long.ok);
    assert!(err(&long).contains("10000"), "{}", err(&long));
    let click = e
        .act(json!({ "action": "click", "query": "#board", "moves": 5 }))
        .await;
    assert!(!click.ok);
    assert!(err(&click).contains("drag"), "{}", err(&click));
    e.finish().await;
}

/// ctrl+wheel is how canvas tools zoom: the wheel event carries ctrlKey and
/// the Control key is pressed around it.
#[tokio::test(flavor = "multi_thread")]
async fn a_ctrl_wheel_reports_ctrl_key() {
    let Some(e) = Env::new().await else {
        return;
    };
    let d = e
        .ok(json!({ "action": "scroll", "query": "#board", "dy": -100, "modifiers": ["ctrl"] }))
        .await;
    assert_eq!(d["modifiers"], json!(["Control"]), "{d}");
    assert_eq!(
        d["scroll_at"]["pixel"],
        json!({"x": 150.0, "y": 100.0}),
        "{d}"
    );
    let wheel = e.json("window.__wheel").await;
    let w = wheel.as_array().expect("wheel events");
    assert_eq!(w.len(), 1, "{wheel}");
    assert_eq!(w[0]["ctrl"], true, "{wheel}");
    assert_eq!(w[0]["shift"], false, "{wheel}");
    assert!(w[0]["dy"].as_f64().is_some_and(|dy| dy < 0.0), "{wheel}");
    let ev = e.events().await;
    let kinds: Vec<&Value> = ev.iter().map(|v| &v["type"]).collect();
    let kd = kinds
        .iter()
        .position(|k| **k == "keydown")
        .expect("keydown");
    let wh = kinds.iter().position(|k| **k == "wheel").expect("wheel");
    let ku = kinds.iter().position(|k| **k == "keyup").expect("keyup");
    assert!(kd < wh && wh < ku, "{kinds:?}");

    // Without the modifier the page sees a plain wheel.
    let d = e
        .ok(json!({ "action": "scroll", "query": "#board", "dy": 100 }))
        .await;
    assert!(d.get("modifiers").is_none(), "{d}");
    let wheel = e.json("window.__wheel").await;
    assert_eq!(wheel[1]["ctrl"], false, "{wheel}");
    e.finish().await;
}

/// A polygon tool is a batch: a click per vertex, then a double click on the
/// last to close it, all in one call.
#[tokio::test(flavor = "multi_thread")]
async fn a_polygon_is_three_clicks_and_a_double_click_in_one_batch() {
    let Some(e) = Env::new().await else {
        return;
    };
    let d = e
        .ok(json!({
            "steps": [
                { "action": "click", "query": "#board", "x": 50, "y": 50 },
                { "action": "click", "query": "#board", "x": 150, "y": 50 },
                { "action": "click", "query": "#board", "x": 100, "y": 120 },
                { "action": "double_click", "query": "#board", "x": 100, "y": 120 }
            ]
        }))
        .await;
    assert_eq!(d["ran"], 4, "{d}");
    assert_eq!(
        d["steps"][3]["click_at"]["pixel"],
        json!({"x": 100.0, "y": 120.0}),
        "{d}"
    );
    let polys = e.json("window.__polygons").await;
    assert_eq!(
        polys.to_string(),
        "[[[50,50],[150,50],[100,120]]]",
        "{polys}"
    );
    assert_eq!(e.json("window.__pending").await, json!([]));
    e.finish().await;
}

/// Dragging a vertex handle resizes the SVG rect it belongs to: the page's
/// pointer capture sees a real press, held moves and a release.
#[tokio::test(flavor = "multi_thread")]
async fn dragging_a_vertex_handle_resizes_the_svg_rect() {
    let Some(e) = Env::new().await else {
        return;
    };
    let d = e
        .ok(json!({ "action": "drag", "query": "#v", "dx": 40, "dy": 30 }))
        .await;
    assert_eq!(d["html5_drag"], false, "{d}");
    let rect = e
        .json("[+document.getElementById('rect').getAttribute('width'), +document.getElementById('rect').getAttribute('height')]")
        .await;
    assert_eq!(rect.to_string(), "[100,90]", "{rect}");
    let handle = e
        .json("[+document.getElementById('v').getAttribute('cx'), +document.getElementById('v').getAttribute('cy')]")
        .await;
    assert_eq!(handle.to_string(), "[110,110]", "{handle}");
    let v = e.json("window.__vertex").await;
    assert_eq!(v["down"], 1, "{v}");
    assert_eq!(v["up"], 1, "{v}");
    assert!(v["moves"].as_u64().is_some_and(|n| n >= 10), "{v}");

    // The handle has moved: a second drag by element finds it where it is.
    e.ok(json!({ "action": "drag", "query": "#v", "dx": -40, "dy": -30 }))
        .await;
    let rect = e
        .json("[+document.getElementById('rect').getAttribute('width'), +document.getElementById('rect').getAttribute('height')]")
        .await;
    assert_eq!(rect.to_string(), "[60,60]", "{rect}");
    e.finish().await;
}

/// On a canvas, x and y are bitmap pixels: the 300x200 board is styled
/// 600x400 behind a 4px border, so pixel (100, 50) is viewport (214, 114),
/// and `click_at`, `hit` and `at` all say which pixel was hit.
#[tokio::test(flavor = "multi_thread")]
async fn canvas_coordinates_are_bitmap_pixels_on_a_scaled_canvas() {
    let Some(e) = Env::new().await else {
        return;
    };
    let d = e
        .ok(json!({ "action": "click", "query": "#board", "x": 100, "y": 50 }))
        .await;
    assert_eq!(d["click_at"]["x"], 214.0, "{d}");
    assert_eq!(d["click_at"]["y"], 114.0, "{d}");
    assert_eq!(
        d["click_at"]["pixel"],
        json!({"x": 100.0, "y": 50.0}),
        "{d}"
    );
    assert_eq!(d["hit"]["tag"], "canvas", "{d}");
    assert_eq!(d["hit"]["canvas"], true, "{d}");
    assert!(
        near(&d["hit"]["pixel"]["x"], 100.0) && near(&d["hit"]["pixel"]["y"], 50.0),
        "{d}"
    );
    let ev = e.events().await;
    let down = of_type(&ev, "pointerdown");
    assert_eq!(down.len(), 1, "{ev:?}");
    assert!(
        near(&down[0]["x"], 100.0) && near(&down[0]["y"], 50.0),
        "{ev:?}"
    );

    // Without a target the point is viewport px; landing on the canvas, the
    // hit still names the pixel.
    e.clear().await;
    let d = e.ok(json!({ "action": "click", "x": 214, "y": 114 })).await;
    assert!(
        near(&d["hit"]["pixel"]["x"], 100.0) && near(&d["hit"]["pixel"]["y"], 50.0),
        "{d}"
    );
    assert!(d["click_at"].get("pixel").is_none(), "no canvas frame: {d}");
    let ev = e.events().await;
    assert!(near(&of_type(&ev, "pointerdown")[0]["x"], 100.0), "{ev:?}");

    // The default point is the middle of the bitmap, and the far corner is
    // still inside: a canvas-sized offset is not off the element.
    e.clear().await;
    let d = e
        .ok(json!({ "action": "mouse_move", "query": "#board" }))
        .await;
    assert_eq!(d["at"]["pixel"], json!({"x": 150.0, "y": 100.0}), "{d}");
    assert_eq!(d["at"]["x"], 314.0, "{d}");
    e.clear().await;
    let d = e
        .ok(json!({ "action": "hover", "query": "#board", "x": 299, "y": 199 }))
        .await;
    assert_eq!(d["hit"]["tag"], "canvas", "{d}");
    assert_eq!(d["click_at"]["x"], 612.0, "{d}");
    let ev = e.events().await;
    assert!(
        near(&of_type(&ev, "pointermove").last().unwrap()["x"], 299.0),
        "{ev:?}"
    );

    // A non-canvas target keeps CSS offsets and reports no pixel.
    let d = e
        .ok(json!({ "action": "click", "query": "#svg", "x": 10, "y": 10 }))
        .await;
    assert_eq!(d["click_at"]["x"], 632.0, "{d}");
    assert!(d["click_at"].get("pixel").is_none(), "{d}");
    assert!(d["hit"].get("pixel").is_none(), "{d}");
    e.finish().await;
}
