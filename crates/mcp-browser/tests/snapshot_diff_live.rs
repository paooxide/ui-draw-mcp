//! Live tests (real headless Chrome) for snapshot diffs and batched
//! `browser_act`.
//!
//! The cost this removes (MiniWoB++ with Haiku, against Playwright MCP): after
//! every action the model asked for the whole page again, and filling a form
//! took one call per field. `browser_snapshot diff` answers with what moved,
//! and `browser_act steps` runs a form in one call. Skipped when
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

/// A signup form under a navigation bar of 12 links. "Show details" opens a
/// section of three more controls. Submit writes what was entered to `#out`.
fn page() -> String {
    let mut nav = String::new();
    for i in 1..=12 {
        nav.push_str(&format!("<a href=\"/other\">Link number {i}</a> "));
    }
    format!(
        r##"<!doctype html><title>Signup</title><body style="margin:10px">
<nav>{nav}</nav>
<form onsubmit="return false">
<label for=name>Full name</label><input id=name placeholder="Full name">
<label for=email>Email</label><input id=email type=email placeholder="Email address">
<input id=pw type=password aria-label="Password">
<label><input id=agree type=checkbox> I agree</label>
<button id=show type=button aria-expanded="false">Show details</button>
<div id=more style="display:none">
  <a href="#terms">Terms of service</a>
  <a href="#privacy">Privacy policy</a>
  <button type=button>Contact support</button>
</div>
<button id=go type=button>Create account</button>
</form>
<div id=out></div>
<script>
document.getElementById('show').onclick = function(){{
  var m = document.getElementById('more');
  var open = m.style.display === 'none';
  m.style.display = open ? 'block' : 'none';
  this.setAttribute('aria-expanded', open ? 'true' : 'false');
}};
document.getElementById('go').onclick = function(){{
  document.getElementById('out').textContent = 'created:' +
    document.getElementById('name').value + '|' + document.getElementById('email').value +
    '|' + document.getElementById('agree').checked;
}};
</script></body>"##
    )
}

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
                        let req = String::from_utf8_lossy(&buf[..n]).to_string();
                        let html = if req.starts_with("GET /other") {
                            "<!doctype html><title>Other</title><button>Elsewhere</button>".to_string()
                        } else {
                            page()
                        };
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            html.len(),
                            html
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

async fn setup() -> Option<(
    Arc<CdpBackend>,
    String,
    BrowserModule,
    String,
    tokio::sync::oneshot::Sender<()>,
)> {
    let (b, t) = tab().await?;
    let (base, stop) = serve().await;
    b.navigate(&t, "goto", Some(&format!("{base}/")))
        .await
        .expect("goto");
    let m = BrowserModule::new(b.clone());
    Some((b, t, m, base, stop))
}

async fn call(m: &BrowserModule, tool: &str, args: Value) -> Envelope {
    m.call(tool, args, &ctx()).await
}

async fn snap(m: &BrowserModule, t: &str, extra: Value) -> Value {
    let mut args = json!({ "target_id": t });
    args.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let e = call(m, "browser_snapshot", args).await;
    assert!(e.ok, "{e:?}");
    e.data.expect("data")
}

fn size(v: &Value) -> usize {
    serde_json::to_string(v).unwrap().len()
}

/// The snapshot as it was before this change: every key spelled out (nulls
/// and empty strings, `is_enabled`) and refs indexed at every step, without
/// the value, checked and expanded keys it lacked.
fn old_shape(snapshot: &Value) -> Value {
    let nodes: Vec<Value> = snapshot["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| {
            let mut o = n.as_object().cloned().unwrap();
            for k in ["value", "checked", "expanded"] {
                o.remove(k);
            }
            for k in ["role", "name", "semantic_intent", "bound_state"] {
                o.entry(k)
                    .or_insert(if k == "name" { json!("") } else { Value::Null });
            }
            let disabled = o.remove("disabled").is_some();
            o.insert("is_enabled".into(), json!(!disabled));
            let r = o["ref"].as_str().unwrap().to_string();
            let r = if r.starts_with("//*") {
                r
            } else {
                r.split('/')
                    .map(|s| {
                        if s.is_empty() || s == "html" || s.ends_with(']') {
                            s.to_string()
                        } else {
                            format!("{s}[1]")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("/")
            };
            o.insert("ref".into(), json!(r));
            Value::Object(o)
        })
        .collect();
    json!({ "url": snapshot["url"], "title": snapshot["title"], "mode": "dom", "nodes": nodes })
}

fn find(nodes: &[Value], f: impl Fn(&Value) -> bool) -> &Value {
    nodes.iter().find(|n| f(n)).expect("node")
}

async fn text(b: &CdpBackend, t: &str, js: &str) -> String {
    b.eval(t, js).await.expect("eval")["result"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// Typing into a field changes one key of one node, and the diff says so and
/// nothing else, in a fraction of the bytes.
#[tokio::test(flavor = "multi_thread")]
async fn diff_after_typing_shows_only_that_value() {
    let Some((b, t, m, _base, stop)) = setup().await else {
        return;
    };
    let full = snap(&m, &t, json!({})).await;
    assert!(full.get("diff").is_none(), "default stays a full snapshot");
    let nodes = full["nodes"].as_array().unwrap();
    let name = find(nodes, |n| n["name"] == "Full name" && n["tag"] == "input");
    assert!(name.get("value").is_none(), "empty value is left out");
    let pw = find(nodes, |n| n["name"] == "Password");

    let r = call(
        &m,
        "browser_act",
        json!({ "target_id": t, "ref": name["ref"], "action": "type", "value": "Ada Lovelace" }),
    )
    .await;
    assert!(r.ok, "{r:?}");
    let r = call(
        &m,
        "browser_act",
        json!({ "target_id": t, "ref": pw["ref"], "action": "type", "value": "hunter2", "secret": true }),
    )
    .await;
    assert!(r.ok, "{r:?}");

    let diff = snap(&m, &t, json!({ "diff": true })).await;
    assert_eq!(diff["diff"], "delta", "{diff}");
    assert!(
        diff.get("added").is_none() && diff.get("removed").is_none(),
        "{diff}"
    );
    assert!(
        diff.get("url").is_none() && diff.get("title").is_none(),
        "{diff}"
    );
    let changed = diff["changed"].as_array().unwrap();
    assert_eq!(
        changed.len(),
        1,
        "a password's value is never listed: {diff}"
    );
    assert_eq!(changed[0]["ref"], name["ref"]);
    assert_eq!(
        changed[0]["changes"]["value"],
        json!([null, "Ada Lovelace"])
    );
    assert_eq!(
        diff["unchanged"].as_u64().unwrap() as usize,
        nodes.len() - 1
    );

    // since: "last" is the same request, and measures against the diff just sent.
    let again = snap(&m, &t, json!({ "since": "last" })).await;
    assert_eq!(again["diff"], "delta");
    assert!(again.get("changed").is_none(), "{again}");

    let (f, d, old) = (size(&full), size(&diff), size(&old_shape(&full)));
    println!(
        "snapshot bytes: old shape {old}, compact full {f}, diff after one field {d} ({} nodes)",
        nodes.len()
    );
    assert!(d * 10 < f, "diff {d} should be a tenth of the snapshot {f}");

    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

/// A click that reveals a section: the new controls are `added`, the toggle's
/// `expanded` is a change, and nothing is reported removed.
#[tokio::test(flavor = "multi_thread")]
async fn diff_after_a_click_lists_the_revealed_nodes() {
    let Some((b, t, m, _base, stop)) = setup().await else {
        return;
    };
    let full = snap(&m, &t, json!({})).await;
    let nodes = full["nodes"].as_array().unwrap();
    let show = find(nodes, |n| n["name"] == "Show details");
    assert_eq!(show["expanded"], false);
    assert_eq!(
        find(nodes, |n| n["tag"] == "input" && n["checked"].is_boolean())["checked"],
        false
    );

    let r = call(
        &m,
        "browser_act",
        json!({ "target_id": t, "ref": show["ref"], "action": "click" }),
    )
    .await;
    assert!(r.ok, "{r:?}");
    let diff = snap(&m, &t, json!({ "diff": true })).await;
    assert_eq!(diff["diff"], "delta", "{diff}");
    let added: Vec<&str> = diff["added"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        added,
        ["Terms of service", "Privacy policy", "Contact support"],
        "{diff}"
    );
    assert!(diff.get("removed").is_none(), "{diff}");
    let changed = diff["changed"].as_array().unwrap();
    assert_eq!(changed.len(), 1, "{diff}");
    assert_eq!(changed[0]["name"], "Show details");
    assert_eq!(changed[0]["changes"]["expanded"], json!([false, true]));
    println!(
        "reveal: full {} bytes, diff {} bytes",
        size(&snap(&m, &t, json!({})).await),
        size(&diff)
    );

    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

/// Without an earlier snapshot, or on another document, a diff request gets
/// the whole page and the reason.
#[tokio::test(flavor = "multi_thread")]
async fn diff_falls_back_to_full_without_history_and_after_navigation() {
    let Some((b, t, m, base, stop)) = setup().await else {
        return;
    };
    let first = snap(&m, &t, json!({ "diff": true })).await;
    assert_eq!(first["diff"], "full", "{first}");
    assert!(first["reason"].as_str().unwrap().contains("no previous"));
    assert!(first["nodes"].as_array().unwrap().len() > 15);

    let n = call(
        &m,
        "browser_navigate",
        json!({ "target_id": t, "url": format!("{base}/other") }),
    )
    .await;
    assert!(n.ok, "{n:?}");
    let after = snap(&m, &t, json!({ "diff": true })).await;
    assert_eq!(after["diff"], "full", "{after}");
    assert!(after["reason"]
        .as_str()
        .unwrap()
        .contains("different document"));
    assert_eq!(after["title"], "Other");
    assert!(after["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n["name"] == "Elsewhere"));

    // The full reply became the baseline.
    let next = snap(&m, &t, json!({ "diff": true })).await;
    assert_eq!(next["diff"], "delta");
    assert_eq!(next["unchanged"], 1);

    // A different scope is not comparable either.
    let scoped = snap(&m, &t, json!({ "diff": true, "root_selector": "body" })).await;
    assert_eq!(scoped["diff"], "full");

    // Text mode has no nodes to compare: the page text, unchanged in shape.
    let txt = snap(&m, &t, json!({ "diff": true, "mode": "text" })).await;
    assert!(txt["text"].as_str().unwrap().contains("Elsewhere"), "{txt}");

    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

/// A form is one call: two fields, a tick and the submit button.
#[tokio::test(flavor = "multi_thread")]
async fn a_batch_fills_a_form_in_one_call() {
    let Some((b, t, m, _base, stop)) = setup().await else {
        return;
    };
    let before = snap(&m, &t, json!({})).await;
    let r = call(
        &m,
        "browser_act",
        json!({
            "target_id": t,
            "steps": [
                { "action": "type", "query": "#name", "value": "Ada Lovelace" },
                { "action": "type", "query": "#email", "value": "ada@example.com" },
                { "action": "click", "query": "#agree" },
                { "action": "click", "query": "#go" }
            ],
            "snapshot": "diff"
        }),
    )
    .await;
    assert!(r.ok, "{r:?}");
    let d = r.data.as_ref().unwrap();
    assert_eq!(d["ran"], 4, "{d}");
    assert_eq!(d["total"], 4);
    assert!(d.get("failed_at").is_none());
    let steps = d["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 4);
    assert!(steps.iter().all(|s| s["ok"] == true), "{d}");
    assert_eq!(steps[0]["value_after"], "Ada Lovelace", "{d}");
    assert_eq!(
        text(&b, &t, "document.getElementById('out').textContent").await,
        "created:Ada Lovelace|ada@example.com|true"
    );

    // The diff after the last step: both fields and the box, nothing else.
    let s = &d["snapshot"];
    assert_eq!(s["diff"], "delta", "{s}");
    let mut seen: Vec<(String, String)> = s["changed"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|c| {
            c["changes"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v[1].to_string()))
        })
        .collect();
    seen.sort();
    assert_eq!(
        seen,
        [
            ("checked", "true"),
            ("value", "\"Ada Lovelace\""),
            ("value", "\"ada@example.com\""),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string())),
        "{s}"
    );
    println!(
        "batch of 4 steps with diff: {} bytes, against a full snapshot of {}",
        size(d),
        size(&before)
    );

    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

/// The first failing step ends the batch; what ran stays done, and the error
/// carries the per-step results.
#[tokio::test(flavor = "multi_thread")]
async fn a_bad_step_stops_the_batch_and_says_where() {
    let Some((b, t, m, _base, stop)) = setup().await else {
        return;
    };
    let r = call(
        &m,
        "browser_act",
        json!({
            "target_id": t,
            "steps": [
                { "action": "type", "query": "#name", "value": "Grace" },
                { "action": "click", "query": "#no-such-button" },
                { "action": "type", "query": "#email", "value": "never@example.com" }
            ]
        }),
    )
    .await;
    assert!(!r.ok, "{r:?}");
    let msg = r.error.as_ref().unwrap().message.clone();
    assert!(msg.starts_with("step 1 (click) failed"), "{msg}");
    let d = r
        .data
        .as_ref()
        .expect("a failed batch still reports its steps");
    assert_eq!(d["ran"], 2, "{d}");
    assert_eq!(d["total"], 3);
    assert_eq!(d["failed_at"], 1);
    let steps = d["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0]["ok"], true);
    assert_eq!(steps[1]["ok"], false);
    assert!(steps[1]["error"]["message"].as_str().unwrap().len() > 3);
    assert_eq!(
        text(&b, &t, "document.getElementById('name').value").await,
        "Grace"
    );
    assert_eq!(
        text(&b, &t, "document.getElementById('email').value").await,
        ""
    );

    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

/// A batch is one call acting on one tab, and each step is checked as a
/// single act is.
#[tokio::test(flavor = "multi_thread")]
async fn a_batch_is_validated() {
    let Some((b, t, m, _base, stop)) = setup().await else {
        return;
    };
    let many: Vec<Value> = (0..21)
        .map(|_| json!({ "action": "focus", "query": "#name" }))
        .collect();
    for (args, needle) in [
        (json!({ "target_id": t, "steps": [] }), "1 to 20"),
        (json!({ "target_id": t, "steps": many }), "1 to 20"),
        (
            json!({ "target_id": t, "steps": ["click"] }),
            "must be an object",
        ),
        (
            json!({ "target_id": t, "steps": [{ "steps": [], "action": "click" }] }),
            "nested",
        ),
        (
            json!({ "target_id": t, "steps": [{ "action": "focus", "query": "#name", "target_id": "other" }] }),
            "one tab",
        ),
        (
            json!({ "target_id": t, "action": "click", "steps": [{ "action": "focus", "query": "#name" }] }),
            "inside each step",
        ),
        (
            json!({ "target_id": t, "steps": [{ "action": "focus", "query": "#name" }], "snapshot": "bogus" }),
            "diff",
        ),
    ] {
        let r = call(&m, "browser_act", args).await;
        assert!(!r.ok, "{r:?}");
        let msg = r.error.as_ref().unwrap().message.clone();
        assert!(msg.contains(needle), "{needle}: {msg}");
    }
    // A step with nothing to act on fails as a single act does.
    let r = call(
        &m,
        "browser_act",
        json!({ "target_id": t, "steps": [{ "action": "click" }] }),
    )
    .await;
    assert!(!r.ok);
    assert_eq!(r.data.as_ref().unwrap()["failed_at"], 0);
    assert!(r.error.unwrap().message.contains("need 'ref'"));

    // A batch with no earlier snapshot returns a full one, saying why.
    let r = call(
        &m,
        "browser_act",
        json!({ "target_id": t, "steps": [{ "action": "focus", "query": "#name" }], "snapshot": "diff" }),
    )
    .await;
    assert!(r.ok, "{r:?}");
    assert_eq!(r.data.unwrap()["snapshot"]["diff"], "full");

    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}
