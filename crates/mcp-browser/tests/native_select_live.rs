//! Live tests (real headless Chrome) for native `<select>` lists through
//! `browser_act`, `browser_fill_form` and `browser_snapshot`.
//!
//! The failure this guards (MiniWoB++ choose-list, three runs out of six):
//! the model clicked `…/option[6]`, or called `select` on it, and both said ok
//! while the list kept its old value. `click()` on an `<option>` selects
//! nothing, and setting `value` on one rewrites its value attribute. An option
//! now stands for its select, the result says what is selected, and a miss is
//! an error that lists the options. Skipped when `AGENTCTL_SKIP_LIVE` is set or
//! no Chrome binary is found.

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

/// `#names` is the choose-list shape: a labelled select whose option values
/// differ from their text, logging every change event to `#log`. `#sticky`
/// puts itself back on change, as a controlled component whose state did not
/// take would. `#city` has a `<datalist>`.
const PAGE: &str = r#"<!doctype html><body style="margin:20px">
<label for="names">Name</label>
<select id="names">
  <option value="g-1">Guendolen</option>
  <option value="t-2">Tonga</option>
  <option value="m-3">  Margret </option>
  <option value="x-4" disabled>Xavier</option>
</select>
<select id="sticky" aria-label="Sticky"><option>One</option><option>Two</option></select>
<input id="city" list="cities"><datalist id="cities"><option value="Paris"></option></datalist>
<div id="log"></div>
<script>
document.getElementById('names').addEventListener('change', function(e){
  var l = document.getElementById('log');
  l.textContent = (l.textContent ? l.textContent + ',' : '') + e.target.value;
});
document.getElementById('sticky').addEventListener('change', function(e){ e.target.selectedIndex = 0; });
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

async fn page(b: &CdpBackend, t: &str, js: &str) -> String {
    b.eval(t, js).await.expect("eval")["result"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

async fn act(
    m: &BrowserModule,
    t: &str,
    action: &str,
    query: &str,
    value: Option<&str>,
) -> Envelope {
    // An XPath is a ref, as browser_snapshot hands them out.
    let key = if query.starts_with('/') {
        "ref"
    } else {
        "query"
    };
    let mut args = json!({ "target_id": t, key: query, "action": action });
    if let Some(v) = value {
        args["value"] = json!(v);
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

fn err(e: &Envelope) -> String {
    e.error.as_ref().expect("error").message.clone()
}

/// The benchmark failures: a click on an option, and select on an option
/// ref, both choose it in the parent list and fire change.
#[tokio::test(flavor = "multi_thread")]
async fn an_option_target_chooses_it_in_its_select() {
    let Some((b, t, m, stop)) = setup().await else {
        return;
    };
    let c = act(&m, &t, "click", "#names option:nth-child(2)", None).await;
    assert!(c.ok, "{c:?}");
    let d = c.data.as_ref().unwrap();
    assert_eq!(d["selected"], "Tonga", "{d}");
    assert_eq!(d["changed"], true, "{d}");
    assert_eq!(d["input"], "synthetic", "{d}");
    assert_eq!(
        page(&b, &t, "document.getElementById('names').value").await,
        "t-2"
    );

    // select on an option ref, with the value the model also passed.
    let s = act(
        &m,
        &t,
        "select",
        "//select[@id='names']/option[3]",
        Some("Margret"),
    )
    .await;
    assert!(s.ok, "{s:?}");
    assert_eq!(s.data.as_ref().unwrap()["selected"], "Margret");
    // ... and with no value at all.
    let s = act(&m, &t, "select", "//select[@id='names']/option[1]", None).await;
    assert!(s.ok, "{s:?}");
    assert_eq!(s.data.as_ref().unwrap()["selected"], "Guendolen");

    // Choosing what is already chosen is ok and says nothing changed.
    let again = act(&m, &t, "click", "#names option:nth-child(1)", None).await;
    assert!(again.ok, "{again:?}");
    assert_eq!(again.data.as_ref().unwrap()["changed"], false);
    assert_eq!(
        page(&b, &t, "document.getElementById('log').textContent").await,
        "t-2,m-3,g-1,g-1"
    );
    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

/// select on the list takes the option's text or its value, either case; a
/// miss, a disabled option and a page that puts the list back are errors.
#[tokio::test(flavor = "multi_thread")]
async fn select_matches_text_or_value_and_reports_misses() {
    let Some((b, t, m, stop)) = setup().await else {
        return;
    };
    for (want, got) in [("Margret", "m-3"), ("t-2", "t-2"), ("guendolen", "g-1")] {
        let s = act(&m, &t, "select", "#names", Some(want)).await;
        assert!(s.ok, "{want}: {s:?}");
        assert_eq!(
            page(&b, &t, "document.getElementById('names').value").await,
            got
        );
    }

    let miss = act(&m, &t, "select", "#names", Some("Nobody")).await;
    assert!(!miss.ok);
    let e = err(&miss);
    assert!(e.contains("no option") && e.contains("Tonga"), "{e}");
    assert_eq!(
        page(&b, &t, "document.getElementById('names').value").await,
        "g-1"
    );

    let off = act(&m, &t, "select", "#names", Some("Xavier")).await;
    assert!(err(&off).contains("disabled"), "{off:?}");

    let sticky = act(&m, &t, "select", "#sticky", Some("Two")).await;
    assert!(err(&sticky).contains("reset"), "{sticky:?}");

    let dl = act(&m, &t, "click", "#cities option", None).await;
    assert!(err(&dl).contains("datalist"), "{dl:?}");
    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

/// A click on the list itself chooses nothing and says what to do instead.
#[tokio::test(flavor = "multi_thread")]
async fn clicking_a_select_points_at_action_select() {
    let Some((b, t, m, stop)) = setup().await else {
        return;
    };
    let c = act(&m, &t, "click", "#names", None).await;
    assert!(c.ok, "{c:?}");
    let d = c.data.as_ref().unwrap();
    assert!(
        d["input_reason"]
            .as_str()
            .unwrap()
            .contains("action select"),
        "{d}"
    );
    assert_eq!(d["selected"], "Guendolen", "{d}");
    assert_eq!(d["options"][2], "Margret", "{d}");
    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

/// The snapshot names a select by its label and lists its options with the
/// selected one, so no browser_query is needed to find the option text.
#[tokio::test(flavor = "multi_thread")]
async fn snapshot_lists_select_options() {
    let Some((b, t, m, stop)) = setup().await else {
        return;
    };
    let s = m
        .call("browser_snapshot", json!({ "target_id": t }), &ctx())
        .await;
    assert!(s.ok, "{s:?}");
    let nodes = s.data.as_ref().unwrap()["nodes"]
        .as_array()
        .unwrap()
        .clone();
    let names = nodes
        .iter()
        .find(|n| n["tag"] == "select" && n["name"] == "Name")
        .unwrap_or_else(|| panic!("no select named Name: {nodes:?}"));
    assert_eq!(names["selected"], "Guendolen", "{names}");
    assert_eq!(
        names["options"],
        json!(["Guendolen", "Tonga", "Margret", "Xavier"]),
        "{names}"
    );
    let other: Vec<&Value> = nodes.iter().filter(|n| n["tag"] == "a").collect();
    assert!(other.iter().all(|n| n.get("options").is_none()));
    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

/// fill_form chooses by text as select does, and a miss is a field error.
#[tokio::test(flavor = "multi_thread")]
async fn fill_form_selects_by_text() {
    let Some((b, t, m, stop)) = setup().await else {
        return;
    };
    let f = m
        .call(
            "browser_fill_form",
            json!({ "target_id": t, "fields": [{ "selector": "#names", "value": "Tonga" }] }),
            &ctx(),
        )
        .await;
    assert!(f.ok, "{f:?}");
    assert_eq!(
        page(&b, &t, "document.getElementById('names').value").await,
        "t-2"
    );

    let f = m
        .call(
            "browser_fill_form",
            json!({ "target_id": t, "fields": [{ "selector": "#names", "value": "Nobody" }] }),
            &ctx(),
        )
        .await;
    let text = serde_json::to_string(&f).unwrap();
    assert!(text.contains("no option"), "{text}");
    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}
