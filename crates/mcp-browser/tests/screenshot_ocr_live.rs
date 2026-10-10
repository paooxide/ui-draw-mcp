//! Live tests (real headless Chrome) for `browser_screenshot` with `ocr` and
//! `find`.
//!
//! The failure this guards: a canvas app, an image-based UI or an annotation
//! tool shows its text as pixels. `browser_snapshot` sees one element and
//! `browser_query` finds nothing, so the model's only move was a screenshot it
//! read with its own eyes, every turn, guessing a coordinate off the picture.
//! `find` must return the word's centre in the viewport CSS px `browser_act`
//! takes, whatever the device scale factor, and clicking there must hit it.
//!
//! The recogniser's two models (about 20 MB) are fetched on the first run
//! into `target/ocr` (or `$AGENTCTL_OCR_MODELS`). Skipped when
//! `AGENTCTL_SKIP_LIVE` is set or no Chrome binary is found.

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mcp_browser::{BrowserBackend, BrowserModule, CdpBackend, NavPolicy, OcrEngine, CHROME_BINS};
use mcp_types::ToolModule;
use mcp_types::{CallCtx, CancelToken, Envelope};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// How far, in CSS px, the OCR centre may sit from the word's true centre.
/// The recogniser's box follows the glyphs' ink, whose vertical middle is a
/// few px off the font's, and a 2x capture halves any pixel error.
const TOLERANCE: f64 = 12.0;

fn skip_live() -> bool {
    std::env::var_os("AGENTCTL_SKIP_LIVE").is_some_and(|v| v != "0")
}

fn have_chrome() -> bool {
    !skip_live() && CHROME_BINS.iter().any(|p| std::path::Path::new(p).exists())
}

/// Where the test keeps the models: beside the build artefacts, so one
/// download serves every run on this machine, or wherever the operator says.
fn model_dir() -> PathBuf {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            p.pop();
            p.pop();
            p.join("target")
        });
    OcrEngine::model_dir(&target)
}

fn fixture() -> String {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.push("docs/fixtures/ocr_canvas.html");
    std::fs::read_to_string(&p).expect("docs/fixtures/ocr_canvas.html")
}

/// A headless Chrome with a large window and the given device scale factor.
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

struct Live {
    b: Arc<CdpBackend>,
    t: String,
    m: BrowserModule,
    _stop: tokio::sync::oneshot::Sender<()>,
}

async fn setup(dsf: u32) -> Option<Live> {
    let (b, t) = tab(dsf).await?;
    let (base, stop) = serve(fixture()).await;
    b.navigate(&t, "goto", Some(&format!("{base}/")))
        .await
        .expect("goto");
    common::wait_until(
        "the canvas to be drawn",
        Duration::from_secs(10),
        || async {
            b.eval(&t, "window.__ready === true")
                .await
                .map(|v| v["result"] == true)
                .unwrap_or(false)
        },
    )
    .await;
    let m = BrowserModule::new(b.clone()).with_ocr_models(model_dir());
    Some(Live {
        b,
        t,
        m,
        _stop: stop,
    })
}

impl Live {
    async fn shot(&self, mut args: Value) -> Envelope {
        args["target_id"] = json!(self.t);
        let e = self.m.call("browser_screenshot", args, &ctx()).await;
        assert!(e.ok, "{e:?}");
        e
    }

    /// The word's true centre in viewport CSS px, as the page knows it.
    async fn word_center(&self, word: &str) -> (f64, f64) {
        let v = self
            .b
            .eval(&self.t, &format!("window.__words[{}]", json!(word)))
            .await
            .expect("eval");
        let r = &v["result"];
        (r["x"].as_f64().unwrap(), r["y"].as_f64().unwrap())
    }

    async fn last_click(&self) -> Value {
        let v = self
            .b
            .eval(
                &self.t,
                "window.__clicks[window.__clicks.length - 1] || null",
            )
            .await
            .expect("eval");
        v["result"].clone()
    }

    async fn dpr(&self) -> f64 {
        let v = self
            .b
            .eval(&self.t, "devicePixelRatio")
            .await
            .expect("eval");
        v["result"].as_f64().unwrap()
    }
}

fn close(a: (f64, f64), b: (f64, f64)) -> bool {
    (a.0 - b.0).abs() <= TOLERANCE && (a.1 - b.1).abs() <= TOLERANCE
}

/// On a 2x display the capture has twice the pixels of the viewport. `find`
/// must still return the word's centre in CSS px, carry no image, and a click
/// at that point must land on the word.
#[tokio::test(flavor = "multi_thread")]
async fn find_returns_a_clickable_css_px_centre_on_a_2x_display() {
    let Some(l) = setup(2).await else {
        return;
    };
    assert_eq!(
        l.dpr().await,
        2.0,
        "the launch flag should give a 2x display"
    );

    let e = l.shot(json!({ "find": "Save" })).await;
    assert!(
        e.image.is_none(),
        "OCR must not return the image unless asked"
    );
    let d = e.data.as_ref().unwrap();
    assert_eq!(d["scale"], 2.0, "{d}");
    assert_eq!(d["coordinate_space"], "viewport", "{d}");
    assert!(d["count"].as_u64().unwrap() >= 1, "{d}");
    assert!(d.get("hint").is_none(), "{d}");
    let m = &d["matches"][0];
    assert_eq!(m["rank"], 1);
    assert_eq!(m["matched"], "line", "the label itself ranks first: {d}");
    let got = (m["x"].as_f64().unwrap(), m["y"].as_f64().unwrap());
    let want = l.word_center("Save").await;
    assert!(
        close(got, want),
        "find gave {got:?}, the word is at {want:?}: {d}"
    );

    // Clicking where OCR said hits the word, through the real pointer.
    let act =
        l.m.call(
            "browser_act",
            json!({ "target_id": l.t, "action": "click", "x": got.0, "y": got.1 }),
            &ctx(),
        )
        .await;
    assert!(act.ok, "{act:?}");
    let click = l.last_click().await;
    assert_eq!(click["word"], "Save", "the click landed elsewhere: {click}");

    // A word with the query inside it ranks below the label that is the query.
    let e = l.shot(json!({ "find": "save" })).await;
    let d = e.data.as_ref().unwrap();
    let texts: Vec<&str> = d["matches"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["text"].as_str())
        .collect();
    assert_eq!(texts.first().copied(), Some("Save"), "{d}");
    assert!(
        texts.iter().any(|t| t.eq_ignore_ascii_case("autosave")),
        "Autosave should match as a fragment: {d}"
    );
}

/// At 1x, `ocr: true` reads every word with its centre in CSS px (the canvas's
/// own 2x backing store does not leak into the coordinates), `image: true`
/// brings the picture back, and a miss is `ok` with `count: 0` and a hint
/// naming what is there instead.
#[tokio::test(flavor = "multi_thread")]
async fn ocr_reads_every_word_and_a_miss_is_count_zero_with_a_hint() {
    let Some(l) = setup(1).await else {
        return;
    };
    let e = l.shot(json!({ "ocr": true })).await;
    assert!(e.image.is_none());
    let d = e.data.as_ref().unwrap();
    assert_eq!(d["scale"], 1.0, "{d}");
    assert_eq!(d["engine"], "ocrs");
    let lines = d["lines"].as_array().unwrap();
    assert!(d["line_count"].as_u64().unwrap() >= 4, "{d}");
    for word in ["Save", "Cancel", "Open file", "Autosave"] {
        let want = l.word_center(word).await;
        let line = lines
            .iter()
            .find(|x| {
                x["text"]
                    .as_str()
                    .is_some_and(|t| t.eq_ignore_ascii_case(word))
            })
            .unwrap_or_else(|| panic!("{word} not read: {d}"));
        let c = &line["center"];
        let got = (c["x"].as_f64().unwrap(), c["y"].as_f64().unwrap());
        assert!(close(got, want), "{word}: centre {got:?}, word at {want:?}");
    }
    assert!(d["text"].as_str().unwrap().contains("Cancel"), "{d}");

    let e = l.shot(json!({ "ocr": true, "image": true })).await;
    assert!(e.image.is_some(), "image=true brings the picture back");

    let e = l.shot(json!({ "find": "Delete" })).await;
    let d = e.data.as_ref().unwrap();
    assert_eq!(d["count"], 0, "{d}");
    assert_eq!(d["matches"], json!([]));
    let hint = d["hint"].as_str().unwrap();
    assert!(hint.contains("nearest:"), "{hint}");
    assert!(hint.contains("Cancel") || hint.contains("Save"), "{hint}");
}

/// A `ref` capture starts at the element; the point still comes back in
/// viewport CSS px (the element's position added), so it clicks as returned.
#[tokio::test(flavor = "multi_thread")]
async fn an_element_shot_reports_viewport_points() {
    let Some(l) = setup(2).await else {
        return;
    };
    let e = l
        .shot(json!({ "ref": "//*[@id=\"page\"]", "find": "Cancel" }))
        .await;
    let d = e.data.as_ref().unwrap();
    assert_eq!(d["coordinate_space"], "viewport", "{d}");
    assert_eq!(d["origin"]["x"], 20.0, "{d}");
    assert_eq!(d["origin"]["y"], 40.0, "{d}");
    assert!(d["count"].as_u64().unwrap() >= 1, "{d}");
    let m = &d["matches"][0];
    let got = (m["x"].as_f64().unwrap(), m["y"].as_f64().unwrap());
    let want = l.word_center("Cancel").await;
    assert!(
        close(got, want),
        "find gave {got:?}, the word is at {want:?}: {d}"
    );
    let act =
        l.m.call(
            "browser_act",
            json!({ "target_id": l.t, "action": "click", "x": got.0, "y": got.1 }),
            &ctx(),
        )
        .await;
    assert!(act.ok, "{act:?}");
    assert_eq!(l.last_click().await["word"], "Cancel");
}
