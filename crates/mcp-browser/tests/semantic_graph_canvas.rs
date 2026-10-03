//! Live tests (real headless Chrome) for page-published canvas regions and the
//! unified semantic graph / shadow DOM.
//!
//! A canvas gets child nodes only when the page itself publishes its regions
//! (`canvas.__agentctl_regions` or `data-canvas-regions`). Acting on one sends
//! real CDP mouse input at the region centre; the pages below only react to
//! trusted events, so a synthetic-`MouseEvent` implementation would fail.

use mcp_browser::{BrowserBackend, BrowserError, CdpBackend, Locator, NavPolicy, CHROME_BINS};
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn skip_live() -> bool {
    std::env::var_os("AGENTCTL_SKIP_LIVE").is_some_and(|v| v != "0")
}

fn have_chrome() -> bool {
    !skip_live() && CHROME_BINS.iter().any(|p| std::path::Path::new(p).exists())
}

async fn tab(port: u64) -> Option<(CdpBackend, String)> {
    if !have_chrome() {
        return None;
    }
    // Allow private / loopback addresses for local test fixtures
    let b = CdpBackend::new(NavPolicy::new(&[], true));
    b.connect(None, Some(json!({ "headless": true, "port": port })))
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
    Some((b, target))
}

fn fixture_content(filename: &str) -> String {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates
    path.pop(); // workspace root
    path.push("docs");
    path.push("fixtures");
    path.push(filename);
    fs::read_to_string(&path).expect("fixture file should exist")
}

async fn serve_html(content: String) -> (String, tokio::sync::oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind listener");
    let addr = listener.local_addr().expect("local addr");
    let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut rx => break,
                res = listener.accept() => {
                    if let Ok((mut stream, _)) = res {
                        let mut buf = [0u8; 1024];
                        let _ = stream.read(&mut buf).await;
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            content.len(),
                            content
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                        let _ = stream.flush().await;
                    }
                }
            }
        }
    });
    (format!("http://127.0.0.1:{}", addr.port()), tx)
}

#[tokio::test(flavor = "multi_thread")]
async fn test_canvas_calculator_published_regions_and_actions() {
    let Some((b, t)) = tab(9490).await else {
        return;
    };

    let html = fixture_content("canvas_calculator.html");
    let (url, _shutdown) = serve_html(html).await;
    b.navigate(&t, "goto", Some(&url))
        .await
        .expect("navigate failed");

    // Let the canvas render
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    // 1. Take snapshot and verify canvas buttons are indexed as interactable refs
    let snap = b.snapshot(&t, "dom", None).await.expect("snapshot failed");
    let nodes = snap
        .get("nodes")
        .and_then(|n| n.as_array())
        .expect("nodes array");

    let btn_7 = nodes
        .iter()
        .find(|n| {
            n.get("ref")
                .and_then(|r| r.as_str())
                .is_some_and(|r| r.contains("::canvas[7]"))
        })
        .expect("Canvas button 7 should be present");
    assert_eq!(
        btn_7.get("tag").and_then(|s| s.as_str()),
        Some("canvas-child")
    );
    assert_eq!(btn_7.get("name").and_then(|s| s.as_str()), Some("7"));
    assert_eq!(btn_7.get("role").and_then(|s| s.as_str()), Some("button"));
    assert_eq!(
        btn_7.get("semantic_intent").and_then(|s| s.as_str()),
        Some("calc_num_7")
    );

    let btn_plus = nodes
        .iter()
        .find(|n| {
            n.get("ref")
                .and_then(|r| r.as_str())
                .is_some_and(|r| r.contains("::canvas[+]"))
        })
        .expect("Canvas button + should be present");

    let btn_3 = nodes
        .iter()
        .find(|n| {
            n.get("ref")
                .and_then(|r| r.as_str())
                .is_some_and(|r| r.contains("::canvas[3]"))
        })
        .expect("Canvas button 3 should be present");

    let btn_eq = nodes
        .iter()
        .find(|n| {
            n.get("ref")
                .and_then(|r| r.as_str())
                .is_some_and(|r| r.contains("::canvas[=]"))
        })
        .expect("Canvas button = should be present");

    let ref_7 = btn_7.get("ref").unwrap().as_str().unwrap();
    let ref_plus = btn_plus.get("ref").unwrap().as_str().unwrap();
    let ref_3 = btn_3.get("ref").unwrap().as_str().unwrap();
    let ref_eq = btn_eq.get("ref").unwrap().as_str().unwrap();

    // 2. Perform sequential acts: 7 + 3 =
    b.act(&t, Locator::Ref(ref_7), "click", None)
        .await
        .expect("click 7");
    b.act(&t, Locator::Ref(ref_plus), "click", None)
        .await
        .expect("click +");
    b.act(&t, Locator::Ref(ref_3), "click", None)
        .await
        .expect("click 3");
    b.act(&t, Locator::Ref(ref_eq), "click", None)
        .await
        .expect("click =");

    // 3. Verify display value updated to 10
    let eval_res = b
        .eval(
            &t,
            "document.getElementById('calcCanvas').getAttribute('data-display-value')",
        )
        .await
        .expect("eval failed");
    let display_val = eval_res
        .get("result")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(
        display_val, "10",
        "Canvas calculation 7 + 3 should equal 10"
    );

    let _ = b.tabs(1, "close", Some(&t), None).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_spa_state_and_shadow_dom_traversal() {
    let Some((b, t)) = tab(9491).await else {
        return;
    };

    let html = fixture_content("spa_cart.html");
    let (url, _shutdown) = serve_html(html).await;
    b.navigate(&t, "goto", Some(&url))
        .await
        .expect("navigate failed");

    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    // 1. Take snapshot and verify semantic intent, bound state, and shadow DOM elements
    let snap = b.snapshot(&t, "dom", None).await.expect("snapshot failed");
    let nodes = snap
        .get("nodes")
        .and_then(|n| n.as_array())
        .expect("nodes array");

    // Checkout button node inspection
    let checkout_node = nodes
        .iter()
        .find(|n| {
            n.get("name")
                .and_then(|s| s.as_str())
                .is_some_and(|name| name.contains("Checkout"))
        })
        .expect("Checkout button must be indexed");

    assert_eq!(
        checkout_node
            .get("semantic_intent")
            .and_then(|s| s.as_str()),
        Some("checkout_order")
    );
    let bound = checkout_node
        .get("bound_state")
        .expect("bound_state must be present");
    assert_eq!(bound.get("count").and_then(|c| c.as_u64()), Some(3));
    assert_eq!(bound.get("total").and_then(|c| c.as_f64()), Some(89.97));
    assert_eq!(
        checkout_node.get("is_enabled").and_then(|b| b.as_bool()),
        Some(true)
    );

    // Shadow DOM button inspection
    let shadow_btn = nodes
        .iter()
        .find(|n| {
            n.get("name")
                .and_then(|s| s.as_str())
                .is_some_and(|name| name.contains("Apply Discount"))
        })
        .expect("Shadow DOM button must be indexed");

    let shadow_ref = shadow_btn.get("ref").and_then(|r| r.as_str()).unwrap();
    assert!(
        shadow_ref.contains("::shadow/"),
        "Shadow DOM element ref must contain ::shadow/ path separator, got: {shadow_ref}"
    );
    assert_eq!(
        shadow_btn.get("semantic_intent").and_then(|s| s.as_str()),
        Some("apply_discount")
    );

    // 2. Act on the shadow DOM button via its ref
    b.act(&t, Locator::Ref(shadow_ref), "click", None)
        .await
        .expect("act on shadow DOM button");

    let eval_res = b
        .eval(
            &t,
            "document.getElementById('orderWidget').getAttribute('data-applied')",
        )
        .await
        .expect("eval failed");
    let applied = eval_res
        .get("result")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(
        applied, "true",
        "Shadow DOM button click handler should have executed"
    );

    let _ = b.tabs(1, "close", Some(&t), None).await;
}

/// A 400x200 bitmap canvas displayed at 200x100 CSS px (so regions in bitmap
/// space must be halved), plus an unpublished canvas. Handlers ignore
/// untrusted events and hit-test offsetX/offsetY in bitmap space.
const SCALED_PAGE: &str = r#"<!DOCTYPE html><html><body style="margin:0">
<div style="height:30px"></div>
<canvas id="c" width="400" height="200" style="display:block;margin:10px 0 0 20px;width:200px;height:100px;border:0;padding:0"></canvas>
<canvas id="plain" width="50" height="50" style="display:block"></canvas>
<script>
const c = document.getElementById('c');
const regions = [
  { id: 'left', x: 0, y: 0, w: 100, h: 100 },
  { id: 'a]b::c"d', label: 'Awkward', x: 300, y: 100, w: 80, h: 60 },
];
c.__agentctl_regions = regions;
c.setAttribute('data-log', '');
function onTrusted(e) {
  if (!e.isTrusted) { c.setAttribute('data-untrusted', '1'); return; }
  const bx = e.offsetX * (c.width / c.clientWidth);
  const by = e.offsetY * (c.height / c.clientHeight);
  for (const r of regions) {
    if (bx >= r.x && bx < r.x + r.w && by >= r.y && by < r.y + r.h) {
      c.setAttribute('data-hit', r.id);
      c.setAttribute('data-log', c.getAttribute('data-log') + e.type + ';');
      return;
    }
  }
  c.setAttribute('data-hit', 'miss');
}
c.addEventListener('pointerdown', onTrusted);
c.addEventListener('click', onTrusted);
</script></body></html>"#;

async fn nodes_of(b: &CdpBackend, t: &str) -> Vec<serde_json::Value> {
    let snap = b.snapshot(t, "dom", None).await.expect("snapshot failed");
    snap.get("nodes")
        .and_then(|n| n.as_array())
        .expect("nodes array")
        .clone()
}

async fn attr(b: &CdpBackend, t: &str, name: &str) -> String {
    let r = b
        .eval(
            t,
            &format!("document.getElementById('c').getAttribute('{name}') || ''"),
        )
        .await
        .expect("eval failed");
    r.get("result")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn node_ref_by_name(nodes: &[serde_json::Value], name: &str) -> String {
    nodes
        .iter()
        .find(|n| n.get("name").and_then(|s| s.as_str()) == Some(name))
        .and_then(|n| n.get("ref").and_then(|r| r.as_str()))
        .unwrap_or_else(|| panic!("no node named {name}"))
        .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn test_canvas_click_is_trusted_scaled_and_awkward_id_round_trips() {
    let Some((b, t)) = tab(9492).await else {
        return;
    };
    let (url, _shutdown) = serve_html(SCALED_PAGE.to_string()).await;
    b.navigate(&t, "goto", Some(&url)).await.expect("navigate");
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    let nodes = nodes_of(&b, &t).await;
    let children: Vec<_> = nodes
        .iter()
        .filter(|n| n.get("tag").and_then(|s| s.as_str()) == Some("canvas-child"))
        .collect();
    assert_eq!(children.len(), 2, "both published regions become nodes");
    // The canvas that publishes nothing stays one opaque `canvas` node.
    let plain = nodes
        .iter()
        .filter(|n| n.get("tag").and_then(|s| s.as_str()) == Some("canvas"))
        .count();
    assert_eq!(plain, 1, "unpublished canvas is a single opaque node");

    let awkward = children
        .iter()
        .find(|n| n.get("name").and_then(|s| s.as_str()) == Some("Awkward"))
        .expect("awkward region node");
    let awkward_ref = awkward.get("ref").and_then(|r| r.as_str()).unwrap();
    // The id is escaped: nothing after the marker can confuse the parser.
    let id_part = awkward_ref.rsplit("::canvas[").next().unwrap();
    assert!(id_part.ends_with(']') && id_part.matches(']').count() == 1);
    assert!(
        !id_part.contains(':') && !id_part.contains('"'),
        "{id_part}"
    );

    // CSS-scaled geometry: bitmap (300,100,80x60) at 0.5 scale.
    let rect = b
        .eval(
            &t,
            "(()=>{const r=document.getElementById('c').getBoundingClientRect();return [r.left,r.top]})()",
        )
        .await
        .expect("rect");
    let rect = rect.get("result").and_then(|v| v.as_array()).unwrap();
    let (left, top) = (rect[0].as_f64().unwrap(), rect[1].as_f64().unwrap());
    assert_eq!(awkward.get("w").and_then(|v| v.as_i64()), Some(40));
    assert_eq!(awkward.get("h").and_then(|v| v.as_i64()), Some(30));
    assert_eq!(
        awkward.get("x").and_then(|v| v.as_i64()),
        Some((left + 150.0).round() as i64)
    );
    assert_eq!(
        awkward.get("y").and_then(|v| v.as_i64()),
        Some((top + 50.0).round() as i64)
    );

    // Click the awkward region: a trusted pointer event must hit it.
    let res = b
        .act(&t, Locator::Ref(awkward_ref), "click", None)
        .await
        .expect("click awkward region");
    assert_eq!(res.get("input").and_then(|v| v.as_str()), Some("cdp"));
    let (rx, ry) = (
        res.get("x").and_then(|v| v.as_f64()).unwrap(),
        res.get("y").and_then(|v| v.as_f64()).unwrap(),
    );
    assert!((rx - (left + 170.0)).abs() < 0.01, "centre x {rx}");
    assert!((ry - (top + 65.0)).abs() < 0.01, "centre y {ry}");
    assert_eq!(attr(&b, &t, "data-hit").await, "a]b::c\"d");
    assert_eq!(
        attr(&b, &t, "data-untrusted").await,
        "",
        "no synthetic events"
    );
    let log = attr(&b, &t, "data-log").await;
    assert!(
        log.contains("pointerdown;") && log.contains("click;"),
        "{log}"
    );

    // The other region too, to prove it is the centre maths and not luck.
    let left_ref = node_ref_by_name(&nodes, "left");
    b.act(&t, Locator::Ref(&left_ref), "click", None)
        .await
        .expect("click left region");
    assert_eq!(attr(&b, &t, "data-hit").await, "left");

    let _ = b.tabs(1, "close", Some(&t), None).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_canvas_region_unsupported_actions_and_hover() {
    let Some((b, t)) = tab(9493).await else {
        return;
    };
    let (url, _shutdown) = serve_html(SCALED_PAGE.to_string()).await;
    b.navigate(&t, "goto", Some(&url)).await.expect("navigate");
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    let nodes = nodes_of(&b, &t).await;
    let region_ref = node_ref_by_name(&nodes, "left");

    for action in ["type", "select", "focus", "submit"] {
        let err = b
            .act(&t, Locator::Ref(&region_ref), action, Some("x"))
            .await
            .expect_err(action);
        assert!(
            matches!(err, BrowserError::Unsupported(_)),
            "{action} on a canvas region must be Unsupported, got {err:?}"
        );
    }
    assert_eq!(attr(&b, &t, "data-hit").await, "", "nothing was clicked");

    // Hover is a real mouseMoved: succeeds, reports CDP input, no click.
    let res = b
        .act(&t, Locator::Ref(&region_ref), "hover", None)
        .await
        .expect("hover");
    assert_eq!(res.get("input").and_then(|v| v.as_str()), Some("cdp"));
    assert_eq!(attr(&b, &t, "data-hit").await, "");

    let _ = b.tabs(1, "close", Some(&t), None).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_canvas_region_outside_viewport_is_an_error() {
    let Some((b, t)) = tab(9494).await else {
        return;
    };
    let html = r#"<!DOCTYPE html><html><body style="margin:0">
<canvas id="c" width="4000" height="4000" style="display:block;width:4000px;height:4000px"></canvas>
<script>
const c = document.getElementById('c');
c.__agentctl_regions = [{ id: 'far', x: 3900, y: 3900, w: 40, h: 40 }];
c.addEventListener('pointerdown', () => c.setAttribute('data-hit', 'yes'));
</script></body></html>"#;
    let (url, _shutdown) = serve_html(html.to_string()).await;
    b.navigate(&t, "goto", Some(&url)).await.expect("navigate");
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    let nodes = nodes_of(&b, &t).await;
    let far_ref = nodes
        .iter()
        .find(|n| n.get("tag").and_then(|s| s.as_str()) == Some("canvas-child"))
        .and_then(|n| n.get("ref").and_then(|r| r.as_str()))
        .expect("region ref")
        .to_string();
    let err = b
        .act(&t, Locator::Ref(&far_ref), "click", None)
        .await
        .expect_err("point is outside the viewport");
    assert!(
        matches!(err, BrowserError::Failed(ref m) if m.contains("outside")),
        "{err:?}"
    );
    assert_eq!(attr(&b, &t, "data-hit").await, "", "nothing was clicked");

    let _ = b.tabs(1, "close", Some(&t), None).await;
}
