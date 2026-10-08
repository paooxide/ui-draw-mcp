//! Live tests (real headless Chrome) for how a locator is read when the model
//! spells it loosely.
//!
//! The failures this guards (MiniWoB++ with Claude Haiku, ~390 runs): a plain
//! word passed as `query` with no `by` was the CSS tag `<gilli>` and found
//! nothing (174 runs); `"Section #1"`, `text=Alanna`, `button:has-text("Go")`,
//! an XPath ref as `query` and a truncated ref were invalid CSS (~50); and the
//! inbox's click-handled divs and icon spans never appeared in a snapshot, so
//! the email could not be sent. Skipped when `AGENTCTL_SKIP_LIVE` is set or no
//! Chrome binary is found.

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

/// `#row` is a click-handled div with a pointer cursor (addEventListener, no
/// role, no onclick property); its span "Gilli" inherits the pointer.
/// `#send-forward` is an icon drawn by `content: url(/send.png)`, as the
/// MiniWoB inbox draws its buttons. `#box` holds one button, for scoping.
const PAGE: &str = r#"<!doctype html><body style="margin:20px">
<h2 id="sec">Section #1</h2>
<div id="row" style="cursor:pointer;padding:6px;border:1px solid #888;width:200px"><span>Gilli</span></div>
<span id="send-forward" style="content:url(/send.png);cursor:pointer;display:inline-block;width:14px;height:14px"></span>
<button id="sub">Submit</button>
<canvas id="cv" width="100" height="40" style="border:1px solid #888"></canvas>
<label for="nm">Name</label><input id="nm">
<div id="box"><button id="inner">Inner</button></div>
<div id="log"></div>
<script>
function note(w){ var l = document.getElementById('log'); l.textContent += w + ';'; }
['row', 'send-forward', 'sub', 'cv'].forEach(function(id){
  document.getElementById(id).addEventListener('click', function(){ note(id); });
});
</script></body>"#;

/// Any bytes will do for the icon; the page only needs its URL to have a name.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xff, 0xff, 0x3f,
    0x00, 0x05, 0xfe, 0x02, 0xfe, 0xa7, 0x35, 0x81, 0x84, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e,
    0x44, 0xae, 0x42, 0x60, 0x82,
];

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
                        let n = stream.read(&mut buf).await.unwrap_or(0);
                        let png = buf[..n].starts_with(b"GET /send.png");
                        let (ctype, body) = if png {
                            ("image/png", PNG.to_vec())
                        } else {
                            ("text/html; charset=utf-8", PAGE.as_bytes().to_vec())
                        };
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(head.as_bytes()).await;
                        let _ = stream.write_all(&body).await;
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

async fn log(b: &CdpBackend, t: &str) -> String {
    b.eval(t, "document.getElementById('log').textContent")
        .await
        .expect("eval")["result"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

async fn click(m: &BrowserModule, t: &str, args: Value) -> Envelope {
    let mut a = json!({ "target_id": t, "action": "click" });
    for (k, v) in args.as_object().unwrap() {
        a[k] = v.clone();
    }
    m.call("browser_act", a, &ctx()).await
}

fn err(e: &Envelope) -> String {
    e.error.as_ref().expect("error").message.clone()
}

async fn nodes(m: &BrowserModule, t: &str, root: Option<&str>) -> Vec<Value> {
    let mut a = json!({ "target_id": t });
    if let Some(r) = root {
        a["root_selector"] = json!(r);
    }
    let s = m.call("browser_snapshot", a, &ctx()).await;
    assert!(s.ok, "{s:?}");
    s.data.as_ref().unwrap()["nodes"]
        .as_array()
        .unwrap()
        .clone()
}

/// The benchmark's commonest failure: a bare word is read as CSS, and as the
/// text it plainly is when it finds nothing. The result says which.
#[tokio::test(flavor = "multi_thread")]
async fn a_bare_word_clicks_the_text() {
    let Some((b, t, m, stop)) = setup().await else {
        return;
    };
    let c = click(&m, &t, json!({ "query": "Gilli" })).await;
    assert!(c.ok, "{c:?}");
    let d = c.data.as_ref().unwrap();
    assert_eq!(d["matched_by"], "text", "{d}");
    assert_eq!(d["target"]["text"], "Gilli", "{d}");
    assert_eq!(log(&b, &t).await, "row;");

    // A selector that parses and matches stays CSS.
    let c = click(&m, &t, json!({ "query": "#sub" })).await;
    assert_eq!(c.data.as_ref().unwrap()["matched_by"], "css", "{c:?}");
    // One that does not parse is the text it reads as.
    let c = click(&m, &t, json!({ "query": "Section #1" })).await;
    assert!(c.ok, "{c:?}");
    let d = c.data.as_ref().unwrap();
    assert_eq!(d["matched_by"], "text", "{d}");
    assert_eq!(d["target"]["text"], "Section #1", "{d}");

    // An explicit `by` is strict and reports nothing extra.
    let strict = click(&m, &t, json!({ "query": "Gilli", "by": "css" })).await;
    assert!(!strict.ok);
    assert_eq!(strict.error.as_ref().unwrap().code, ErrorCode::NotFound);
    assert!(
        err(&strict).contains(r#"no element matches the CSS selector "Gilli""#),
        "{}",
        err(&strict)
    );
    let t_ok = click(&m, &t, json!({ "query": "Gilli", "by": "text" })).await;
    assert!(t_ok.data.as_ref().unwrap().get("matched_by").is_none());
    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

/// The other spellings seen in the transcripts, each meaning what it says.
#[tokio::test(flavor = "multi_thread")]
async fn other_spellings_mean_what_they_say() {
    let Some((b, t, m, stop)) = setup().await else {
        return;
    };
    let c = click(&m, &t, json!({ "query": "text=Gilli" })).await;
    assert!(c.ok, "{c:?}");
    assert_eq!(log(&b, &t).await, "row;");
    // No matched_by: the caller was explicit.
    assert!(c.data.as_ref().unwrap().get("matched_by").is_none());

    let c = click(&m, &t, json!({ "query": r#"button:has-text("Submit")"# })).await;
    assert!(c.ok, "{c:?}");
    assert_eq!(c.data.as_ref().unwrap()["matches"], 1);
    let c = click(&m, &t, json!({ "query": "button:contains('Sub')" })).await;
    assert!(c.ok, "{c:?}");
    assert_eq!(log(&b, &t).await, "row;sub;sub;");

    // An XPath passed as the query, also with the quotes over-escaped.
    for q in [r#"//*[@id="cv"]"#, r#"//*[@id=\"cv\"]"#] {
        let c = click(&m, &t, json!({ "query": q, "by": "css" })).await;
        assert!(c.ok, "{q}: {c:?}");
    }
    assert!(log(&b, &t).await.ends_with("cv;cv;"));

    // A snapshot ref without its /html/body/ front: row is the first div.
    let c = click(&m, &t, json!({ "query": "div[1]/span" })).await;
    assert!(c.ok, "{c:?}");
    assert_eq!(c.data.as_ref().unwrap()["target"]["text"], "Gilli");
    assert!(log(&b, &t).await.ends_with("row;"));

    // `within` takes the same spellings.
    let c = click(
        &m,
        &t,
        json!({ "query": "Inner", "within": "xpath=//div[@id='box']" }),
    )
    .await;
    assert!(c.ok, "{c:?}");
    assert_eq!(c.data.as_ref().unwrap()["target"]["text"], "Inner");
    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn query_and_fill_form_read_the_same_spellings() {
    let Some((b, t, m, stop)) = setup().await else {
        return;
    };
    let q = |args: Value| {
        let m = &m;
        let t = &t;
        async move {
            let mut a = json!({ "target_id": t });
            for (k, v) in args.as_object().unwrap() {
                a[k] = v.clone();
            }
            m.call("browser_query", a, &ctx()).await
        }
    };
    let r = q(json!({ "query": "Gilli" })).await;
    assert!(r.ok, "{r:?}");
    let d = r.data.as_ref().unwrap();
    assert_eq!(
        (d["count"].clone(), d["matched_by"].clone()),
        (json!(1), json!("text"))
    );
    let r = q(json!({ "query": "#sub" })).await;
    assert_eq!(r.data.as_ref().unwrap()["matched_by"], "css");
    let r = q(json!({ "query": r#"button:has-text("Inner")"#, "all": true })).await;
    let d = r.data.as_ref().unwrap();
    assert_eq!(d["count"], 1, "{d}");
    assert!(d.get("matched_by").is_none(), "{d}");
    let r = q(json!({ "query": "//*[@id='sub']" })).await;
    assert_eq!(r.data.as_ref().unwrap()["count"], 1);
    // An empty result is still an answer, not an error.
    let r = q(json!({ "query": "Nobody" })).await;
    assert_eq!(r.data.as_ref().unwrap()["count"], 0);

    // A field by its label text; the label stands for its control.
    let f = m
        .call(
            "browser_fill_form",
            json!({
                "target_id": t,
                "fields": [{ "selector": "text=Name", "value": "Ada" }],
                "submit": { "selector": r#"button:has-text("Submit")"# }
            }),
            &ctx(),
        )
        .await;
    assert!(f.ok, "{f:?}");
    assert_eq!(
        b.eval(&t, "document.getElementById('nm').value")
            .await
            .unwrap()["result"],
        "Ada"
    );
    assert_eq!(log(&b, &t).await, "sub;");
    // A bare word works as a field selector too (auto), and a miss names the field.
    let f = m
        .call(
            "browser_fill_form",
            json!({ "target_id": t, "fields": [{ "selector": "Name", "value": "Bo" }] }),
            &ctx(),
        )
        .await;
    assert!(f.ok, "{f:?}");
    let f = m
        .call(
            "browser_fill_form",
            json!({ "target_id": t, "fields": [{ "selector": "Surname", "value": "x" }] }),
            &ctx(),
        )
        .await;
    let text = serde_json::to_string(&f).unwrap();
    assert!(
        text.contains("Surname") && text.contains("no element matches"),
        "{text}"
    );
    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

/// A click-handled div and an icon span are in the snapshot, once each, and
/// the icon is named after its file (`/send.png` is "send", ahead of the
/// id's "send forward") and found by that name.
#[tokio::test(flavor = "multi_thread")]
async fn the_snapshot_lists_pointer_targets_and_names_icons() {
    let Some((b, t, m, stop)) = setup().await else {
        return;
    };
    let all = nodes(&m, &t, None).await;
    let row: Vec<&Value> = all.iter().filter(|n| n["name"] == "Gilli").collect();
    assert_eq!(row.len(), 1, "the div once, not its span too: {all:?}");
    assert_eq!(row[0]["tag"], "div");
    let icon: Vec<&Value> = all.iter().filter(|n| n["name"] == "send").collect();
    assert_eq!(icon.len(), 1, "{all:?}");
    assert_eq!(icon[0]["tag"], "span");
    // Names that already had text are as they were.
    assert!(all
        .iter()
        .any(|n| n["tag"] == "button" && n["name"] == "Submit"));
    assert!(!all.iter().any(|n| n["tag"] == "h2"), "{all:?}");

    // What the snapshot calls it, a text query finds.
    let r = m
        .call(
            "browser_query",
            json!({ "target_id": t, "by": "text", "query": "send" }),
            &ctx(),
        )
        .await;
    let d = r.data.as_ref().unwrap();
    assert_eq!(d["count"], 1, "{d}");
    assert_eq!(d["matches"][0]["ref"], icon[0]["ref"], "{d}");
    // ... and so does the id's wording, through auto.
    let c = click(&m, &t, json!({ "query": "send" })).await;
    assert!(c.ok, "{c:?}");
    assert_eq!(c.data.as_ref().unwrap()["matched_by"], "text");
    assert_eq!(log(&b, &t).await, "send-forward;");
    let c = click(&m, &t, json!({ "ref": icon[0]["ref"] })).await;
    assert!(c.ok, "{c:?}");
    assert_eq!(log(&b, &t).await, "send-forward;send-forward;");
    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn root_selector_takes_an_xpath_ref_or_css() {
    let Some((b, t, m, stop)) = setup().await else {
        return;
    };
    for root in ["//div[@id='box']", "xpath=//div[@id='box']", "#box"] {
        let inside = nodes(&m, &t, Some(root)).await;
        assert_eq!(inside.len(), 1, "{root}: {inside:?}");
        assert_eq!(inside[0]["name"], "Inner");
    }
    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_miss_says_what_was_tried() {
    let Some((b, t, m, stop)) = setup().await else {
        return;
    };
    let miss = click(&m, &t, json!({ "query": "Nobody" })).await;
    assert!(!miss.ok);
    assert_eq!(miss.error.as_ref().unwrap().code, ErrorCode::NotFound);
    assert_eq!(
        err(&miss),
        r#"no element matches "Nobody" (as a CSS selector or as visible text)"#
    );

    // A word of the query that is on the page is offered back.
    let near = click(&m, &t, json!({ "query": "Gilli Cooper" })).await;
    let e = err(&near);
    assert!(e.contains("similar:") && e.contains(r#""Gilli""#), "{e}");

    let by_text = click(&m, &t, json!({ "query": "Nobody", "by": "text" })).await;
    assert!(err(&by_text).starts_with(r#"no element has the visible text "Nobody""#));
    let by_xpath = click(&m, &t, json!({ "query": "//nobody" })).await;
    assert!(err(&by_xpath).starts_with(r#"no element matches the XPath "//nobody""#));

    let gone = click(&m, &t, json!({ "ref": "/html/body/div[9]/span" })).await;
    assert_eq!(gone.error.as_ref().unwrap().code, ErrorCode::NotFound);
    let e = err(&gone);
    assert!(
        e.contains("no element at ref /html/body/div[9]/span") && e.contains("browser_snapshot"),
        "{e}"
    );

    // A jQuery pseudo-class that cannot be rewritten still gets its hint.
    let hint = click(&m, &t, json!({ "query": "div :contains('Gilli')" })).await;
    assert_eq!(hint.error.as_ref().unwrap().code, ErrorCode::InvalidArgs);
    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}
