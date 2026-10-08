//! Live tests (real headless Chrome) for `browser_screenshot` with `grid`.
//!
//! The failure this guards (MiniWoB++ click-pie, count-shape, tic-tac-toe):
//! the model has to click where it *saw* something, and it estimates pixel
//! coordinates badly from a bare image. The grid is drawn on the returned PNG
//! in the CSS pixels `browser_act` takes, whatever the device scale factor, so
//! it reads a number instead of guessing one. It must not touch the page.
//! Skipped when `AGENTCTL_SKIP_LIVE` is set or no Chrome binary is found.

use std::sync::Arc;

use base64::Engine;
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

/// A headless Chrome with a large window and the given device scale factor.
/// Set at launch: an emulated viewport lives on one DevTools connection, and
/// each tool call opens its own.
async fn tab(dsf: u32) -> Option<(Arc<CdpBackend>, String)> {
    if !have_chrome() {
        return None;
    }
    let b = CdpBackend::new(NavPolicy::new(&[], true));
    let launch = json!({
        "headless": true,
        "port": 0,
        "args": ["--window-size=900,800", format!("--force-device-scale-factor={dsf}")],
    });
    b.connect(None, Some(launch)).await.ok()?;
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

/// A white page with a dark panel on its right half and a blue box, so the
/// grid is checked over light, dark and coloured backgrounds. Nothing here
/// scrolls, so viewport and page coordinates are the same.
const PAGE: &str = r#"<!doctype html><html><body style="margin:0;background:#fff;overflow:hidden">
<div id="dark" style="position:absolute;left:300px;top:0;width:300px;height:400px;background:#111"></div>
<div id="box" style="position:absolute;left:230px;top:130px;width:200px;height:100px;background:#0088ff"></div>
</body></html>"#;

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

async fn setup(
    dsf: u32,
) -> Option<(
    Arc<CdpBackend>,
    String,
    BrowserModule,
    tokio::sync::oneshot::Sender<()>,
)> {
    let (b, t) = tab(dsf).await?;
    let (base, stop) = serve().await;
    b.navigate(&t, "goto", Some(&format!("{base}/")))
        .await
        .expect("goto");
    let m = BrowserModule::new(b.clone());
    Some((b, t, m, stop))
}

/// The viewport in CSS px and the device pixel ratio, as the page sees them.
/// The window size is only a request, so the tests read what they got.
async fn viewport(b: &CdpBackend, t: &str) -> (u32, u32, f64) {
    let v = b
        .eval(t, "[innerWidth, innerHeight, devicePixelRatio]")
        .await
        .expect("eval");
    let r = &v["result"];
    let (w, h, d) = (r[0].as_u64(), r[1].as_u64(), r[2].as_f64());
    (w.unwrap() as u32, h.unwrap() as u32, d.unwrap())
}

async fn shot(m: &BrowserModule, args: Value) -> Envelope {
    let e = m.call("browser_screenshot", args, &ctx()).await;
    assert!(e.ok, "{e:?}");
    e
}

/// Decoded RGB pixels of the image block.
struct Img {
    w: u32,
    h: u32,
    px: Vec<[u8; 3]>,
}

impl Img {
    fn at(&self, x: u32, y: u32) -> [u8; 3] {
        self.px[(y * self.w + x) as usize]
    }
}

fn decode(e: &Envelope) -> Img {
    let b64 = &e.image.as_ref().expect("image").base64;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .expect("base64");
    let mut dec = png::Decoder::new(std::io::Cursor::new(bytes));
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut r = dec.read_info().expect("png");
    let mut buf = vec![0; r.output_buffer_size().expect("size")];
    let info = r.next_frame(&mut buf).expect("frame");
    let n = info.color_type.samples();
    let px = buf[..info.buffer_size()]
        .chunks_exact(n)
        .map(|p| [p[0], p[1], p[2]])
        .collect();
    Img {
        w: info.width,
        h: info.height,
        px,
    }
}

async fn dom_fingerprint(b: &CdpBackend, t: &str) -> String {
    let v = b
        .eval(
            t,
            "document.documentElement.outerHTML + '|' + document.querySelectorAll('*').length",
        )
        .await
        .expect("eval");
    v["result"].as_str().unwrap_or_default().to_string()
}

/// At 1x the image is the viewport, lines are `step` pixels apart, and the
/// result says so. A line differs from the page on white and on dark alike, a
/// pixel beside it is untouched, and the page has no new nodes.
#[tokio::test(flavor = "multi_thread")]
async fn a_1x_grid_is_drawn_on_the_image_and_not_on_the_page() {
    let Some((b, t, m, stop)) = setup(1).await else {
        return;
    };
    let (vw, vh, dpr) = viewport(&b, &t).await;
    assert_eq!(dpr, 1.0);
    assert!(vw >= 600 && vh >= 400, "window too small: {vw}x{vh}");
    let before = dom_fingerprint(&b, &t).await;

    let e = shot(&m, json!({ "target_id": t, "grid": true })).await;
    let d = e.data.as_ref().unwrap();
    assert_eq!(d["grid_step"], 100, "{d}");
    assert_eq!(d["scale"], 1.0, "{d}");
    assert_eq!(d["coordinate_space"], "viewport", "{d}");
    let img = decode(&e);
    assert_eq!((img.w, img.h), (vw, vh), "image is the viewport");
    assert_eq!(
        (d["width"].as_u64(), d["height"].as_u64()),
        (Some(vw as u64), Some(vh as u64))
    );

    // Row 350 is clear of the crossing labels (they sit just below y=300, 400).
    // x=100 is on white: the line differs from the page and its neighbours do not.
    assert_ne!(img.at(100, 350), WHITE, "x=100 line on white");
    assert_eq!(img.at(99, 350), WHITE);
    assert_eq!(img.at(101, 350), WHITE);
    // x=500 is inside the dark panel (x 300..600): the line is lighter than it.
    assert_ne!(img.at(500, 350), DARK, "x=500 line on dark");
    assert_eq!(img.at(499, 350), DARK);
    // Between lines the page shows through.
    assert_eq!(img.at(150, 350), WHITE);
    // y=200 is a horizontal line.
    assert_ne!(img.at(50, 200), WHITE, "y=200 line");

    assert_eq!(
        dom_fingerprint(&b, &t).await,
        before,
        "the page must be exactly as it was"
    );

    // Without grid the image is the plain page.
    let plain = shot(&m, json!({ "target_id": t })).await;
    assert_eq!(decode(&plain).at(100, 350), WHITE);
    assert!(plain.data.as_ref().unwrap().get("scale").is_none());
    stop.send(()).ok();
}

const WHITE: [u8; 3] = [255, 255, 255];
const DARK: [u8; 3] = [17, 17, 17];

/// At 2x the picture has twice the pixels but the same labels: lines are
/// `2 * step` pixels apart, and the result reports scale 2.
#[tokio::test(flavor = "multi_thread")]
async fn a_2x_display_draws_lines_every_two_step_pixels_labelled_in_css_px() {
    let Some((b, t, m, stop)) = setup(2).await else {
        return;
    };
    let (vw, vh, dpr) = viewport(&b, &t).await;
    assert_eq!(dpr, 2.0, "the launch flag should give a 2x display");
    let e = shot(&m, json!({ "target_id": t, "grid": true, "grid_step": 50 })).await;
    let d = e.data.as_ref().unwrap();
    assert_eq!(d["scale"], 2.0, "{d}");
    assert_eq!(d["grid_step"], 50, "{d}");
    let img = decode(&e);
    assert_eq!((img.w, img.h), (vw * 2, vh * 2));
    // x=50 CSS px is pixel 100; pixel 150 (x=75) is not a line. Row 760 is
    // between the horizontal lines (every 100 px) and clear of the labels at
    // the crossings, which are about 30 px tall.
    assert_ne!(img.at(100, 760), WHITE, "x=50 is pixel 100");
    assert_eq!(img.at(150, 760), WHITE, "pixel 150 is x=75");
    assert_eq!(img.at(99, 760), WHITE);
    // x=100 is pixel 200.
    assert_ne!(img.at(200, 760), WHITE, "x=100 is pixel 200");
    stop.send(()).ok();
}

/// An element's picture starts at the element, but the labels are where a click
/// would land: its position in the viewport, not 0, 50, 100 from its corner.
#[tokio::test(flavor = "multi_thread")]
async fn an_element_shot_is_labelled_with_its_viewport_position() {
    let Some((_b, t, m, stop)) = setup(1).await else {
        return;
    };
    let e = shot(
        &m,
        json!({ "target_id": t, "ref": "//*[@id=\"box\"]", "grid": true, "grid_step": 50 }),
    )
    .await;
    let d = e.data.as_ref().unwrap();
    assert_eq!(d["coordinate_space"], "viewport", "{d}");
    assert_eq!(d["origin"]["x"], 230.0, "{d}");
    assert_eq!(d["origin"]["y"], 130.0, "{d}");
    let img = decode(&e);
    assert_eq!((img.w, img.h), (200, 100));
    // x=250 is 20 px into a box whose left edge is x=230; y=150 is 20 px down.
    // Row 60 is below the edge labels along the top and between the y lines;
    // column 100 is right of the ones down the left and between the x lines.
    let blue = img.at(60, 60);
    assert_ne!(img.at(20, 60), blue, "x=250 line");
    assert_eq!(img.at(21, 60), blue);
    assert_ne!(img.at(100, 20), blue, "y=150 line");
    assert_eq!(img.at(100, 21), blue);
    stop.send(()).ok();
}
