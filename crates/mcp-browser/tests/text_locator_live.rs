//! Live tests (real headless Chrome) for `browser_act` / `browser_query` with
//! `by: "text"`: the match a person would mean must come first.
//!
//! The failure this guards (a MiniWoB++ click-button page): the instruction
//! `Click on the "next" button.` precedes `<button>next</button>`, and the old
//! locator took the first element whose text held the query, so `next` clicked
//! the instruction and said ok. Also `ok` hit an `okay` button, and `Next` did
//! not match `next`. Skipped when `AGENTCTL_SKIP_LIVE` is set or no Chrome
//! binary is found.

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

/// The MiniWoB click-button page: an instruction, then the buttons. Every
/// click is counted per control in `#counts` (`okay=1,Next=1`). `#send` is a
/// submit input outside a form; `#pw` holds a value that must never be echoed.
const PAGE: &str = r#"<!doctype html><body style="margin:20px">
<div id="query">Click on the "next" button.</div>
<button id="b-okay">okay</button>
<button id="b-ok">ok</button>
<button id="b-prev">previous</button>
<button id="b-next"><span>Next</span></button>
<input id="send" type="submit" value="Send">
<input id="pw" type="password" name="password" value="s3cret-pw-value">
<div id="counts"></div>
<script>
var counts = {};
function hit(name){ counts[name] = (counts[name] || 0) + 1;
  document.getElementById('counts').textContent = Object.keys(counts).map(function(k){ return k + '=' + counts[k]; }).join(','); }
[['query','query'],['b-okay','okay'],['b-ok','ok'],['b-prev','previous'],['b-next','Next'],['send','send']].forEach(function(p){
  document.getElementById(p[0]).addEventListener('click', function(){ hit(p[1]); });
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

async fn counts(b: &CdpBackend, t: &str) -> String {
    b.eval(t, "document.getElementById('counts').textContent")
        .await
        .expect("eval")["result"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

async fn act(m: &BrowserModule, t: &str, by: &str, query: &str, extra: Value) -> Envelope {
    let mut args = json!({ "target_id": t, "by": by, "query": query, "action": "click" });
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

/// The benchmark failure: `next` is the button, not the sentence that quotes
/// it, and the case of the label does not matter.
#[tokio::test(flavor = "multi_thread")]
async fn text_prefers_the_clickable_exact_match() {
    let Some((b, t, m, stop)) = setup().await else {
        return;
    };
    let c = act(&m, &t, "text", "next", json!({})).await;
    assert!(c.ok, "{c:?}");
    let d = c.data.as_ref().unwrap();
    assert_eq!(d["target"]["tag"], "button", "{d}");
    assert_eq!(d["target"]["text"], "Next", "{d}");
    assert_eq!(d["matches"], 1, "exact matches only: {d}");
    assert_eq!(counts(&b, &t).await, "Next=1");

    // `ok` is the `ok` button, not `okay`; `okay` is `okay`.
    let c = act(&m, &t, "text", "ok", json!({})).await;
    assert!(c.ok, "{c:?}");
    assert_eq!(counts(&b, &t).await, "Next=1,ok=1");
    let c = act(&m, &t, "text", "okay", json!({})).await;
    assert!(c.ok, "{c:?}");
    assert_eq!(counts(&b, &t).await, "Next=1,ok=1,okay=1");
    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

/// A phrase only the instruction holds still finds it; `index` walks the
/// ranked list (clickable before not) rather than document order.
#[tokio::test(flavor = "multi_thread")]
async fn text_falls_back_to_substring_and_index_follows_rank() {
    let Some((b, t, m, stop)) = setup().await else {
        return;
    };
    let c = act(&m, &t, "text", "Click on the", json!({})).await;
    assert!(c.ok, "{c:?}");
    let d = c.data.as_ref().unwrap();
    assert_eq!(d["target"]["tag"], "div", "{d}");
    assert_eq!(counts(&b, &t).await, "query=1");

    // `ne` is only a substring: the Next button (clickable) outranks the
    // instruction although the instruction comes first in the page.
    let c = act(&m, &t, "text", "ne", json!({})).await;
    let d = c.data.as_ref().unwrap();
    assert_eq!(d["target"]["tag"], "button", "{d}");
    assert_eq!(d["matches"], 2, "{d}");
    let c = act(&m, &t, "text", "ne", json!({ "index": 1 })).await;
    let d = c.data.as_ref().unwrap();
    assert_eq!(d["target"]["tag"], "div", "{d}");
    assert_eq!(counts(&b, &t).await, "query=2,Next=1");

    // browser_query ranks the same way.
    let q = m
        .call(
            "browser_query",
            json!({ "target_id": t, "by": "text", "query": "ok", "all": true }),
            &ctx(),
        )
        .await;
    let d = q.data.as_ref().unwrap();
    assert_eq!(d["count"], 1, "{d}");
    assert_eq!(d["matches"][0]["name"], "ok", "{d}");
    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

/// A submit input is found by its caption, and a field is reported by its
/// name, never its value.
#[tokio::test(flavor = "multi_thread")]
async fn text_finds_input_captions_and_target_never_shows_values() {
    let Some((b, t, m, stop)) = setup().await else {
        return;
    };
    let c = act(&m, &t, "text", "send", json!({})).await;
    assert!(c.ok, "{c:?}");
    let d = c.data.as_ref().unwrap();
    assert_eq!(d["target"]["tag"], "input", "{d}");
    assert_eq!(counts(&b, &t).await, "send=1");

    let c = act(&m, &t, "css", "#pw", json!({ "action": "focus" })).await;
    assert!(c.ok, "{c:?}");
    let d = c.data.as_ref().unwrap();
    assert_eq!(d["target"]["text"], "password", "{d}");
    assert_eq!(d["matches"], 1, "{d}");
    assert!(!d.to_string().contains("s3cret"), "{d}");
    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}
