//! Live tests (real headless Chrome) for the forgiving arguments: connect
//! returns the tabs, `target_id` may be left out or be a `browser_id`, a
//! backslash left before an XPath quote is dropped, `type`/`press` go to the
//! focused element, `browser_fill_form` takes XPath and text selectors, and a
//! jQuery pseudo-class is explained.
//!
//! Each is a failure a model made in a MiniWoB++ dry run (see eval/README.md).
//! Skipped when `AGENTCTL_SKIP_LIVE` is set or no Chrome binary is found.

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

const PAGE: &str = r#"<!doctype html><body style="margin:20px">
<div id="tt" style="padding:8px" onclick="log('tt')">target</div>
<input id="name" type="text">
<input id="username" type="text">
<label for="email">E-mail</label><input id="email" type="text">
<button id="close" onclick="log('close')">close&times;</button>
<div id="log"></div>
<script>
function log(s){ document.getElementById('log').textContent += s + ';'; }
document.getElementById('name').addEventListener('keydown', function(e){ log('key:' + e.key); });
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

struct Session {
    b: Arc<CdpBackend>,
    m: BrowserModule,
    stop: tokio::sync::oneshot::Sender<()>,
    /// The connect result.
    connected: Value,
}

impl Session {
    async fn call(&self, tool: &str, args: Value) -> Envelope {
        self.m.call(tool, args, &ctx()).await
    }

    /// The page's own state: the value of an element, or the event log.
    async fn eval(&self, expr: &str) -> String {
        let e = self
            .call("browser_eval", json!({ "expression": expr }))
            .await;
        assert!(e.ok, "{e:?}");
        e.data.unwrap()["result"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    async fn finish(self) {
        let _ = self.stop.send(());
        let _ = self.b.disconnect(1, true).await;
    }
}

/// Connect through the tool (no `browser_tabs`, no target_id from here on) and
/// open the test page.
async fn session() -> Option<Session> {
    if !have_chrome() {
        return None;
    }
    let b = Arc::new(CdpBackend::new(NavPolicy::new(&[], true)));
    let m = BrowserModule::new(b.clone());
    let c = m
        .call(
            "browser_connect",
            json!({ "launch": { "headless": true, "port": 0 } }),
            &ctx(),
        )
        .await;
    assert!(c.ok, "{c:?}");
    let (base, stop) = serve().await;
    let s = Session {
        b,
        m,
        stop,
        connected: c.data.unwrap(),
    };
    let n = s
        .call("browser_navigate", json!({ "url": format!("{base}/") }))
        .await;
    assert!(n.ok, "{n:?}");
    Some(s)
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_lists_the_tabs_and_later_calls_need_no_target() {
    let Some(s) = session().await else {
        return;
    };
    let tabs = s.connected["tabs"].as_array().expect("tabs");
    assert!(!tabs.is_empty(), "{}", s.connected);
    for t in tabs {
        assert!(t["target_id"].is_string() && t.get("title").is_some() && t["url"].is_string());
    }
    let active = s.connected["target_id"].as_str().expect("target_id");
    assert_eq!(tabs[0]["target_id"], active);

    // The navigate above had no target_id and said which tab it used.
    let n = s
        .call("browser_navigate", json!({ "action": "reload" }))
        .await;
    assert!(n.ok, "{n:?}");
    assert_eq!(n.data.unwrap()["target_id"], active);

    // The browser_id the model sees in the connect result stands for its active
    // tab, as a string or a number.
    for id in [json!("1"), json!(1)] {
        let snap = s
            .call(
                "browser_snapshot",
                json!({ "target_id": id, "mode": "text" }),
            )
            .await;
        assert!(snap.ok, "{snap:?}");
        let d = snap.data.unwrap();
        assert_eq!(d["target_id"], active, "{d}");
        assert!(d.to_string().contains("E-mail"), "{d}");
    }
    let shot = s
        .call("browser_screenshot", json!({ "target_id": "1" }))
        .await;
    assert!(shot.ok, "{shot:?}");

    // A tab id named explicitly is used as given and not echoed back.
    let snap = s
        .call(
            "browser_snapshot",
            json!({ "target_id": active, "mode": "text" }),
        )
        .await;
    assert!(snap.ok, "{snap:?}");
    assert!(snap.data.unwrap().get("target_id").is_none());

    // A wrong id is an answer, not a guess.
    let bad = s
        .call("browser_snapshot", json!({ "target_id": "7" }))
        .await;
    let err = bad.error.expect("error");
    assert_eq!(err.code, ErrorCode::NotFound);
    assert!(err.message.contains("connected: 1"), "{}", err.message);
    s.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn act_takes_an_escaped_ref_and_types_into_the_focused_field() {
    let Some(s) = session().await else {
        return;
    };
    // The model left a literal backslash before each quote.
    let c = s
        .call(
            "browser_act",
            json!({ "action": "click", "ref": r#"//*[@id=\"tt\"]"# }),
        )
        .await;
    assert!(c.ok, "{c:?}");
    assert_eq!(
        s.eval("document.getElementById('log').textContent").await,
        "tt;"
    );
    // The same through an XPath query and an XPath `within`.
    let c = s
        .call(
            "browser_act",
            json!({ "action": "click", "by": "xpath", "query": r#"//*[@id=\"tt\"]"# }),
        )
        .await;
    assert!(c.ok, "{c:?}");
    let q = s
        .call(
            "browser_query",
            json!({ "by": "xpath", "query": r#"//input[@id=\'name\']"# }),
        )
        .await;
    assert_eq!(q.data.unwrap()["count"], 1);

    // Nothing is focused yet.
    let e = s
        .call(
            "browser_act",
            json!({ "action": "type", "value": "Slovakia" }),
        )
        .await;
    assert!(!e.ok);
    assert!(
        e.error.unwrap().message.contains("nothing is focused"),
        "type with no focus must say so"
    );

    // Focus the field, then type and press with no ref or query.
    let f = s
        .call(
            "browser_act",
            json!({ "action": "focus", "query": "#name" }),
        )
        .await;
    assert!(f.ok, "{f:?}");
    let t = s
        .call(
            "browser_act",
            json!({ "action": "type", "value": "Slovakia" }),
        )
        .await;
    assert!(t.ok, "{t:?}");
    assert_eq!(t.data.as_ref().unwrap()["input"], "cdp", "{t:?}");
    assert_eq!(
        s.eval("document.getElementById('name').value").await,
        "Slovakia"
    );
    let p = s
        .call(
            "browser_act",
            json!({ "action": "press", "value": "Enter" }),
        )
        .await;
    assert!(p.ok, "{p:?}");
    let log = s.eval("document.getElementById('log').textContent").await;
    assert!(log.ends_with("key:Enter;"), "{log}");
    s.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn fill_form_takes_xpath_and_text_selectors() {
    let Some(s) = session().await else {
        return;
    };
    let f = s
        .call(
            "browser_fill_form",
            json!({ "fields": [
                { "selector": r#"//*[@id=\"username\"]"#, "by": "xpath", "value": "ada" },
                { "selector": "//input[@id='name']", "value": "Ada L" },
                { "selector": "E-mail", "by": "text", "value": "ada@example.com" }
            ] }),
        )
        .await;
    assert!(f.ok, "{f:?}");
    assert_eq!(
        s.eval("document.getElementById('username').value").await,
        "ada"
    );
    assert_eq!(
        s.eval("document.getElementById('name').value").await,
        "Ada L"
    );
    assert_eq!(
        s.eval("document.getElementById('email').value").await,
        "ada@example.com"
    );
    s.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_jquery_pseudo_class_is_explained() {
    let Some(s) = session().await else {
        return;
    };
    let q = s
        .call(
            "browser_query",
            json!({ "query": "button :contains('\u{d7}')", "by": "css" }),
        )
        .await;
    let err = q.error.expect("error");
    assert_eq!(err.code, ErrorCode::InvalidArgs);
    assert!(err.message.contains("valid selector"), "{}", err.message);
    assert!(err
        .suggestion
        .as_deref()
        .unwrap_or("")
        .contains("by: \"text\""));

    let a = s
        .call(
            "browser_act",
            json!({ "action": "click", "query": "button :has-text('close')" }),
        )
        .await;
    assert_eq!(a.error.expect("error").code, ErrorCode::InvalidArgs);

    let f = s
        .call(
            "browser_fill_form",
            json!({ "fields": [{ "selector": "input:eq(0)", "value": "x" }] }),
        )
        .await;
    let err = f.error.expect("error");
    assert_eq!(err.code, ErrorCode::InvalidArgs);
    assert!(err.suggestion.unwrap().contains("by: \"text\""));

    // What the suggestion says to do works.
    let c = s
        .call(
            "browser_act",
            json!({ "action": "click", "by": "text", "query": "close" }),
        )
        .await;
    assert!(c.ok, "{c:?}");
    assert!(s
        .eval("document.getElementById('log').textContent")
        .await
        .contains("close;"));
    s.finish().await;
}
