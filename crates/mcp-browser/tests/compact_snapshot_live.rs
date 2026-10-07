//! Live tests (real headless Chrome) for the compact snapshot: nodes carry no
//! null or empty keys, refs drop `[1]` where a tag has no same-tag sibling, and
//! the old fully indexed refs (saved in flows and checkpoints) still resolve.
//! Skipped when `AGENTCTL_SKIP_LIVE` is set or no Chrome binary is found.

use std::sync::Arc;

use mcp_browser::{BrowserBackend, CdpBackend, Locator, NavPolicy, CHROME_BINS};
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

/// A form of 30 fields in nested divs, a table of buttons, a disabled input,
/// and a shadow root (host `#host`) with two same-tag buttons. Clicks record
/// the control's `name` (or its text) in `window.__last`. The controls have no
/// ids, so their refs are positional XPaths.
fn page() -> String {
    let mut fields = String::new();
    for i in 0..30 {
        fields.push_str(&format!(
            "<div class=row><label>Field {i}</label><div><input name=f{i} placeholder=\"field {i}\"></div></div>"
        ));
    }
    format!(
        r#"<!doctype html><body style="margin:10px">
<div><form><div>{fields}</div><div><input name=off disabled value=x><button type=button>Save</button></div></form></div>
<table><tbody>
<tr><td><button>r1a</button></td><td><button>r1b</button></td></tr>
<tr><td><button>r2a</button></td><td><button>r2b</button></td></tr>
</tbody></table>
<div id=host></div>
<script>
document.addEventListener('click', function(e){{ var t=e.composedPath()[0]; window.__last = t.getAttribute('name') || t.textContent; }}, true);
var sr = document.getElementById('host').attachShadow({{mode:'open'}});
sr.innerHTML = '<div><button>sh1</button><button>sh2</button></div>';
</script></body>"#
    )
}

async fn serve(html: String) -> (String, tokio::sync::oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut rx => break,
                res = listener.accept() => {
                    let Ok((mut stream, _)) = res else { continue };
                    let html = html.clone();
                    tokio::spawn(async move {
                        let mut buf = [0u8; 2048];
                        let _ = stream.read(&mut buf).await;
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

/// The ref as the generator wrote it before refs were shortened: every step of
/// a positional path indexed (but `html`). Id-anchored refs start with `//*`.
fn old_style(r: &str) -> String {
    r.split("::shadow/")
        .map(|seg| {
            if seg.starts_with("//*") {
                return seg.to_string();
            }
            seg.split('/')
                .map(|s| {
                    if s.is_empty() || s == "html" || s.ends_with(']') {
                        s.to_string()
                    } else {
                        format!("{s}[1]")
                    }
                })
                .collect::<Vec<_>>()
                .join("/")
        })
        .collect::<Vec<_>>()
        .join("::shadow/")
}

/// What a node cost before: null and empty fields spelled out, `is_enabled`
/// always present, refs fully indexed.
fn old_node(n: &Value) -> Value {
    let mut o = n.as_object().cloned().unwrap_or_default();
    for k in ["role", "name", "semantic_intent", "bound_state"] {
        if !o.contains_key(k) {
            o.insert(k.into(), if k == "name" { json!("") } else { Value::Null });
        }
    }
    let disabled = o.remove("disabled").is_some();
    o.insert("is_enabled".into(), json!(!disabled));
    let r = o["ref"].as_str().unwrap_or_default().to_string();
    o.insert("ref".into(), json!(old_style(&r)));
    Value::Object(o)
}

async fn last(b: &CdpBackend, t: &str) -> String {
    b.eval(t, "window.__last").await.expect("eval")["result"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_nodes_are_compact_and_refs_short() {
    let Some((b, t)) = tab().await else { return };
    let (base, stop) = serve(page()).await;
    b.navigate(&t, "goto", Some(&format!("{base}/")))
        .await
        .expect("goto");
    let snap = b.snapshot(&t, "dom", None).await.expect("snapshot");
    let nodes = snap["nodes"].as_array().expect("nodes").clone();
    assert!(nodes.len() > 36, "{} nodes", nodes.len());

    for n in &nodes {
        let o = n.as_object().unwrap();
        for (k, v) in o {
            assert!(!v.is_null(), "null {k} in {n}");
            assert_ne!(v, &json!(""), "empty {k} in {n}");
        }
        assert!(!o.contains_key("is_enabled"), "{n}");
        assert_ne!(o.get("disabled"), Some(&json!(false)), "{n}");
    }
    let flagged = nodes.iter().filter(|n| n.get("disabled").is_some()).count();
    assert_eq!(flagged, 1, "only the disabled input is flagged");
    assert!(nodes
        .iter()
        .any(|n| n["tag"] == "input" && n["disabled"] == true));

    // Lone elements have no index; table rows and cells keep theirs.
    let refs: Vec<&str> = nodes.iter().map(|n| n["ref"].as_str().unwrap()).collect();
    assert!(
        refs.iter()
            .any(|r| r.starts_with("/html/body/div[1]/form/div[1]/div[1]/div/input")),
        "{refs:?}"
    );
    assert!(refs
        .iter()
        .any(|r| r.ends_with("/table/tbody/tr[2]/td[2]/button")));
    assert!(refs.iter().any(|r| r.ends_with("::shadow/div/button[2]")));
    assert!(refs.iter().all(|r| !r.contains("body[1]")), "{refs:?}");

    let new_bytes = serde_json::to_vec(&snap["nodes"]).unwrap().len();
    let old: Vec<Value> = nodes.iter().map(old_node).collect();
    let old_bytes = serde_json::to_vec(&old).unwrap().len();
    eprintln!(
        "snapshot of {} nodes: {old_bytes} bytes before (reconstructed), {new_bytes} bytes now",
        nodes.len()
    );
    assert!(new_bytes < old_bytes);
    let _ = stop.send(());
}

#[tokio::test(flavor = "multi_thread")]
async fn short_and_old_refs_resolve_to_the_same_element() {
    let Some((b, t)) = tab().await else { return };
    let (base, stop) = serve(page()).await;
    b.navigate(&t, "goto", Some(&format!("{base}/")))
        .await
        .expect("goto");
    let snap = b.snapshot(&t, "dom", None).await.expect("snapshot");
    let nodes = snap["nodes"].as_array().expect("nodes").clone();

    let mut checked = 0;
    for n in &nodes {
        if n.get("disabled").is_some() {
            continue;
        }
        let short = n["ref"].as_str().unwrap();
        let old = old_style(short);
        b.eval(&t, "window.__last = null").await.expect("reset");
        b.act(&t, Locator::Ref(short), "click", None)
            .await
            .unwrap_or_else(|e| panic!("short ref {short}: {e:?}"));
        let a = last(&b, &t).await;
        b.eval(&t, "window.__last = null").await.expect("reset");
        b.act(&t, Locator::Ref(&old), "click", None)
            .await
            .unwrap_or_else(|e| panic!("old ref {old}: {e:?}"));
        let c = last(&b, &t).await;
        assert!(!a.is_empty(), "{short} clicked nothing");
        assert_eq!(a, c, "{short} vs {old}");
        checked += 1;
    }
    assert!(checked > 36, "{checked}");

    // Sibling indices still pick the right sibling, in the light DOM and in
    // a shadow root (short and old spelling).
    for (r, want) in [
        ("/html/body/table/tbody/tr[2]/td[1]/button", "r2a"),
        ("/html/body/table/tbody/tr[1]/td[2]/button", "r1b"),
        ("//*[@id=\"host\"]::shadow/div/button[2]", "sh2"),
        ("//*[@id=\"host\"]::shadow/div[1]/button[1]", "sh1"),
    ] {
        b.act(&t, Locator::Ref(r), "click", None)
            .await
            .unwrap_or_else(|e| panic!("{r}: {e:?}"));
        assert_eq!(last(&b, &t).await, want, "{r}");
    }
    let _ = stop.send(());
}
