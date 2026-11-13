//! Live tests (real headless Chrome) for saying when an action left a dialog or
//! layer in the way. The page is `docs/fixtures/session_popup.html`.
//!
//! The failure this guards, from MiniWoB++ `login-user-popup`:
//! `browser_fill_form` returned ok with `submitted: true` although focusing a
//! field had raised a popup (OK / Cancel) over the form and disabled it, so the
//! form was never submitted, and the model answered DONE. Skipped when
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

fn fixture(name: &str) -> String {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates
    path.pop(); // workspace root
    path.push("docs/fixtures");
    path.push(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
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
    async fn new(page: &str, query: &str) -> Option<Env> {
        let (b, t) = tab().await?;
        let (base, stop) = serve(fixture(page)).await;
        b.navigate(&t, "goto", Some(&format!("{base}/{query}")))
            .await
            .expect("goto");
        let m = BrowserModule::new(b.clone());
        Some(Env { b, t, m, stop })
    }

    async fn call(&self, tool: &str, mut args: Value) -> Envelope {
        args["target_id"] = json!(self.t);
        self.m.call(tool, args, &ctx()).await
    }

    async fn ok(&self, tool: &str, args: Value) -> Value {
        let e = self.call(tool, args.clone()).await;
        assert!(e.ok, "{tool} {args} -> {e:?}");
        e.data.expect("data")
    }

    async fn js(&self, js: &str) -> Value {
        self.b.eval(&self.t, js).await.expect("eval")["result"].clone()
    }

    async fn finish(self) {
        let _ = self.stop.send(());
        let _ = self.b.disconnect(1, true).await;
    }
}

fn overlay(d: &Value) -> &Value {
    &d["effects"]["overlay"]
}

async fn log(e: &Env, key: &str) -> i64 {
    e.js(&format!("window.L.{key}"))
        .await
        .as_i64()
        .unwrap_or(-1)
}

const FIELDS: fn() -> Value = || {
    json!([
        { "selector": "#username", "value": "ann" },
        { "selector": "#password", "value": "pw" },
    ])
};

#[tokio::test(flavor = "multi_thread")]
async fn fill_form_does_not_claim_a_submit_a_popup_disabled() {
    let Some(e) = Env::new("session_popup.html", "?mode=disable").await else {
        return;
    };
    let d = e
        .ok(
            "browser_fill_form",
            json!({ "fields": FIELDS(), "submit": { "selector": "#subbtn" } }),
        )
        .await;
    assert_eq!(d["submitted"], false, "{d}");
    assert_eq!(d["submit_blocked"], "disabled", "{d}");
    let o = overlay(&d);
    assert!(
        o["text"]
            .as_str()
            .unwrap_or_default()
            .contains("session is about to expire"),
        "{d}"
    );
    assert!(
        o["ref"].as_str().unwrap_or_default().contains("popup"),
        "{d}"
    );
    assert!(o["covers"].as_u64().unwrap_or(0) >= 1, "{d}");
    assert_eq!(log(&e, "submits").await, 0, "the form was never submitted");

    // Dismissing the popup frees the form: now the same call submits.
    e.ok(
        "browser_act",
        json!({ "action": "click", "query": "#popup-cancel" }),
    )
    .await;
    let d = e
        .ok(
            "browser_fill_form",
            json!({ "fields": FIELDS(), "submit": { "selector": "#subbtn" } }),
        )
        .await;
    assert_eq!(d["submitted"], true, "{d}");
    assert!(
        d.get("submit_blocked").is_none() && d.get("effects").is_none(),
        "nothing to say: {d}"
    );
    assert_eq!(log(&e, "submits").await, 1);
    e.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn fill_form_reports_a_submit_a_popup_covers() {
    let Some(e) = Env::new("session_popup.html", "?mode=cover").await else {
        return;
    };
    let d = e
        .ok(
            "browser_fill_form",
            json!({ "fields": FIELDS(), "submit": { "selector": "#subbtn" } }),
        )
        .await;
    assert_eq!(d["submitted"], false, "{d}");
    assert!(
        d["submit_blocked"]
            .as_str()
            .unwrap_or_default()
            .starts_with("covered by \"Your session"),
        "{d}"
    );
    assert!(overlay(&d).is_object(), "{d}");
    assert_eq!(log(&e, "submits").await, 0);
    e.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_form_without_a_popup_submits_and_says_nothing_more() {
    let Some(e) = Env::new("session_popup.html", "").await else {
        return;
    };
    let d = e
        .ok(
            "browser_fill_form",
            json!({ "fields": FIELDS(), "submit": { "selector": "#subbtn" } }),
        )
        .await;
    assert_eq!(d["submitted"], true, "{d}");
    assert!(
        d.get("effects").is_none() && d.get("submit_blocked").is_none(),
        "{d}"
    );
    assert_eq!(log(&e, "submits").await, 1);
    e.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn act_reports_the_layer_a_click_raised_over_the_page() {
    let Some(e) = Env::new("session_popup.html", "?mode=disable").await else {
        return;
    };
    let d = e
        .ok(
            "browser_act",
            json!({ "action": "click", "query": "#username" }),
        )
        .await;
    let o = overlay(&d);
    assert!(
        o["text"]
            .as_str()
            .unwrap_or_default()
            .contains("Exit to home page?"),
        "{d}"
    );
    assert!(o["covers"].as_u64().unwrap_or(0) >= 1, "{d}");

    e.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_modal_dialog_is_an_overlay_and_a_toast_is_not() {
    let Some(e) = Env::new("session_popup.html", "").await else {
        return;
    };
    // Through `steps`, each step reports its own.
    let d = e
        .ok(
            "browser_act",
            json!({ "steps": [
                { "action": "click", "query": "#toastbtn" },
                { "action": "click", "query": "#dlg" },
            ] }),
        )
        .await;
    assert!(
        d["steps"][0]["effects"].get("overlay").is_none(),
        "a small status line is not in the way: {d}"
    );
    let o = overlay(&d["steps"][1]);
    assert!(
        o["text"]
            .as_str()
            .unwrap_or_default()
            .contains("Really leave?"),
        "{d}"
    );
    e.finish().await;
}
