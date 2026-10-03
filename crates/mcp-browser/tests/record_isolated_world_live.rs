//! Live test (real headless Chrome): page script cannot forge recorded steps.
//!
//! The recorder and its `Runtime` binding live in an isolated world, so the
//! page's own JavaScript can neither call the binding nor reach the
//! recorder's state, while the recorder still sees real clicks and typing.
//!
//! Skipped when `AGENTCTL_SKIP_LIVE` is set or no Chrome binary is found.
//! CDP port 9510.

use std::sync::Arc;

use mcp_browser::{BrowserBackend, CdpBackend, Locator, NavPolicy, RecordManager, CHROME_BINS};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn skip_live() -> bool {
    std::env::var_os("AGENTCTL_SKIP_LIVE").is_some_and(|v| v != "0")
}

fn have_chrome() -> bool {
    !skip_live() && CHROME_BINS.iter().any(|p| std::path::Path::new(p).exists())
}

async fn tab(port: u64) -> Option<(Arc<CdpBackend>, String)> {
    if !have_chrome() {
        return None;
    }
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
    Some((Arc::new(b), target))
}

/// Serves `content` for every path.
async fn serve_html(content: String) -> (String, tokio::sync::oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut rx => break,
                res = listener.accept() => {
                    let Ok((mut stream, _)) = res else { continue };
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
    });
    (format!("http://127.0.0.1:{port}"), tx)
}

async fn js(b: &CdpBackend, target: &str, expr: &str) -> Value {
    let env = b.eval(target, expr).await.expect("eval");
    env.get("result")
        .cloned()
        .expect("eval envelope has result")
}

async fn act(b: &CdpBackend, t: &str, css: &str, action: &str, value: Option<&str>) {
    b.act(
        t,
        Locator::Selector {
            by: "css",
            query: css,
            within: None,
            text: None,
            index: None,
        },
        action,
        value,
    )
    .await
    .unwrap_or_else(|e| panic!("act {action} on {css}: {e:?}"));
}

/// The page tries every way it can think of to forge or eavesdrop: it calls
/// the binding by name, replaces it with its own function, pokes at the
/// recorder's state, and keeps what it saw in `window.__probe`.
const PAGE: &str = r##"<!doctype html><html><body>
<input id="name">
<button id="go">Go</button>
<script>
window.__stolen = [];
window.__probe = {};
// An eavesdropper installed before and after the recorder.
window.__agentctl_rec = function (p) { window.__stolen.push(p); };
function forge() {
  var p = window.__probe;
  p.binding_type = typeof window.__agentctl_rec;
  p.recorder_state = typeof window.__agentctl_recorder;
  p.listening_flag = typeof window.__agentctl_recorder_listening;
  try {
    window.__agentctl_rec(JSON.stringify({kind: 'click', tag: 'BUTTON', selector: '#forged-click', text: 'Forged', value: null, key: null, url: location.href}));
    window.__agentctl_rec(JSON.stringify({kind: 'input', tag: 'INPUT', selector: '#forged-input', value: 'forged', url: location.href}));
    p.call = 'called';
  } catch (e) { p.call = 'threw'; }
  try { window.__agentctl_recorder.active = false; p.disable = 'ok'; } catch (e) { p.disable = 'threw'; }
}
forge();
document.getElementById('go').addEventListener('click', forge);
</script>
</body></html>"##;

fn selectors(events: &[mcp_browser::RawInteractionEvent]) -> Vec<String> {
    events
        .iter()
        .filter(|e| e.kind != "navigate")
        .map(|e| format!("{}:{}", e.kind, e.selector))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn page_script_cannot_forge_or_eavesdrop_on_a_recording() {
    let Some((b, t)) = tab(9510).await else {
        return;
    };
    let (base, stop) = serve_html(PAGE.to_string()).await;
    b.navigate(&t, "goto", Some(&base)).await.expect("navigate");

    RecordManager::start(b.as_ref(), &t).await.expect("start");

    // The binding and the recorder's state are not in the page's world. The
    // only `__agentctl_rec` the page sees is the one it defined itself.
    assert_eq!(
        js(&b, &t, "typeof window.__agentctl_recorder").await,
        "undefined",
        "recorder state must not be reachable from the page"
    );
    assert_eq!(
        js(
            &b,
            &t,
            "window.__agentctl_rec.toString().includes('__stolen')"
        )
        .await,
        true,
        "the page's own function must be the only binding it can see"
    );

    // Real input is seen; page script runs `forge()` on every click as well.
    act(&b, &t, "#name", "type", Some("Ada")).await;
    act(&b, &t, "#go", "click", None).await;
    assert_eq!(js(&b, &t, "window.__probe.call").await, "called");

    // A second document (the new-document path, not the install-now path).
    b.navigate(&t, "goto", Some(&format!("{base}/second")))
        .await
        .expect("second page");
    assert_eq!(
        js(&b, &t, "typeof window.__agentctl_recorder").await,
        "undefined",
        "recorder state must not be reachable on later documents either"
    );
    act(&b, &t, "#go", "click", None).await;

    // The page's eavesdropper only ever received the page's own forged calls
    // (forge() calls the page-defined function): never one of the recorder's
    // events, such as the real click or the typed value.
    assert_eq!(
        js(
            &b,
            &t,
            "window.__stolen.filter(function (p) { return p.indexOf('forged') < 0; }).length"
        )
        .await,
        0,
        "page script must not receive the recorder's events"
    );

    let events = RecordManager::stop_raw(b.as_ref(), &t).await.expect("stop");
    let seen = selectors(&events);
    assert!(
        seen.iter().any(|s| s == "click:#go"),
        "the real click is recorded: {seen:?}"
    );
    assert!(
        seen.iter().any(|s| s.starts_with("input:#name")),
        "the real typing is recorded: {seen:?}"
    );
    assert!(
        !seen.iter().any(|s| s.contains("forged")),
        "forged steps must not reach the recording: {seen:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| e.kind == "navigate"
                && e.url.as_deref().is_some_and(|u| u.ends_with("/second"))),
        "navigation to the second document is recorded"
    );

    assert_eq!(
        js(
            &b,
            &t,
            "!!document.getElementById('agentctl-recorder-badge')"
        )
        .await,
        false,
        "stopping takes the badge down"
    );

    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}
