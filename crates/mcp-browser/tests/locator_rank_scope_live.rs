//! Live tests (real headless Chrome) for how a plain-word query ranks its text
//! matches and how `within` scopes a search. The page is
//! `docs/fixtures/rows_and_scopes.html`.
//!
//! The failures this guards, both from MiniWoB++ `email-inbox`: `browser_act`
//! `click query:"Gilli"` (no `by`) matched the sender's name twice, once in
//! the instruction sentence and once in the inbox row, and clicked the
//! sentence, because a row that is only a div with a pointer cursor and a
//! script handler did not count as clickable, so document order decided. And
//! `within:".email-thread"` searched only the first of the many rows. Skipped
//! when `AGENTCTL_SKIP_LIVE` is set or no Chrome binary is found.

use std::path::PathBuf;
use std::sync::Arc;

use mcp_browser::{BrowserBackend, BrowserModule, CdpBackend, NavPolicy, CHROME_BINS};
use mcp_types::ToolModule;
use mcp_types::{CallCtx, CancelToken, Envelope, ErrorCode};
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

    async fn act(&self, args: Value) -> Envelope {
        self.call("browser_act", args).await
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

/// The id of the control (or element) the last click landed in.
async fn last_click(e: &Env) -> String {
    e.js("window.L[window.L.length - 1]")
        .await
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plain_word_prefers_the_clickable_row_to_prose() {
    let Some(e) = Env::new("rows_and_scopes.html", "").await else {
        return;
    };
    // The instruction names Gilli first; the row is a div with a pointer
    // cursor. Both are exact matches and both are visible.
    let d = e
        .ok(
            "browser_act",
            json!({ "action": "click", "query": "Gilli" }),
        )
        .await;
    assert_eq!(d["matched_by"], "text", "{d}");
    assert_eq!(d["matches"], 2, "the count is kept: {d}");
    assert_eq!(last_click(&e).await, "row-gilli", "{d}");

    // `index` walks the same ranked list: the prose is second.
    e.ok(
        "browser_act",
        json!({ "action": "click", "query": "Gilli", "index": 1 }),
    )
    .await;
    assert_eq!(last_click(&e).await, "p-gilli");

    // by: text ranks the same way.
    e.ok(
        "browser_act",
        json!({ "action": "click", "by": "text", "query": "Gilli" }),
    )
    .await;
    assert_eq!(last_click(&e).await, "row-gilli");

    // Nothing clickable among the matches: document order, as before.
    e.ok(
        "browser_act",
        json!({ "action": "click", "query": "Solo mention" }),
    )
    .await;
    assert_eq!(last_click(&e).await, "tie-a");

    // A real link still beats the sentence that mentions it.
    e.ok("browser_act", json!({ "action": "click", "query": "Docs" }))
        .await;
    assert_eq!(last_click(&e).await, "docs-link");

    // Visible before hidden, however clickable the hidden one is.
    e.ok(
        "browser_act",
        json!({ "action": "click", "query": "Hidden" }),
    )
    .await;
    assert_eq!(last_click(&e).await, "vis-prose");

    // Exact before partial: the exact prose beats a clickable partial.
    e.ok(
        "browser_act",
        json!({ "action": "click", "query": "Exact" }),
    )
    .await;
    assert_eq!(last_click(&e).await, "exact-prose");
    e.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn within_searches_every_root_it_matches() {
    let Some(e) = Env::new("rows_and_scopes.html", "").await else {
        return;
    };
    // The target is in the second of three `.card`s.
    let d = e
        .ok(
            "browser_act",
            json!({ "action": "click", "query": "Zed", "within": ".card" }),
        )
        .await;
    assert_eq!(d["matched_by"], "text", "{d}");
    assert_eq!(d["matches"], 1, "{d}");

    // CSS in every root, in document order, and `index` picks among them.
    let d = e
        .ok(
            "browser_act",
            json!({ "action": "click", "by": "css", "query": ".act", "within": ".card", "index": 1 }),
        )
        .await;
    assert_eq!(d["matches"], 3, "{d}");
    assert_eq!(last_click(&e).await, "act2");

    // XPath roots, with a relative query.
    let d = e
        .ok(
            "browser_act",
            json!({ "action": "click", "by": "xpath", "query": ".//button", "within": "//div[@class='card']", "index": 2 }),
        )
        .await;
    assert_eq!(d["matches"], 3, "{d}");
    assert_eq!(last_click(&e).await, "act3");

    // The scope still limits the search: Gilli is not in any card.
    let miss = e
        .act(json!({ "action": "click", "query": "Gilli", "within": ".card" }))
        .await;
    assert!(!miss.ok, "{miss:?}");
    assert_eq!(miss.error.as_ref().unwrap().code, ErrorCode::NotFound);

    // A scope that matches nothing is still an error, not a wider search.
    let none = e
        .act(json!({ "action": "click", "query": "Zed", "within": ".nothing" }))
        .await;
    assert!(!none.ok);
    assert!(
        none.error
            .as_ref()
            .unwrap()
            .message
            .contains("'within' root not found"),
        "{none:?}"
    );
    e.finish().await;
}
