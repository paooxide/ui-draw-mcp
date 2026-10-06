//! Live tests (real headless Chrome) for `browser_upload`: a file set on a file
//! input must reach the page as a trusted `change` event with the real name and
//! size, whether the input is targeted directly or through its `<label>`.
//!
//! Skipped when `AGENTCTL_SKIP_LIVE` is set or no Chrome binary is found.

mod common;

use std::sync::Arc;
use std::time::Duration;

use mcp_browser::{BrowserBackend, BrowserModule, CdpBackend, NavPolicy, CHROME_BINS};
use mcp_types::{CallCtx, CancelToken, Envelope, ToolModule};
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

/// One page: a hidden single-file input inside a label (`#single`), a hidden
/// `multiple` input (`#multi`) and a plain button. Every `change` is recorded
/// in `window.log` with what the page can see of it.
const PAGE: &str = r#"<!doctype html><body>
<label id="pick">Choose a CV <input id="single" type="file" style="display:none"></label>
<input id="multi" type="file" multiple style="display:none">
<button id="btn">not a file input</button>
<script>
window.log = [];
for (const id of ['single', 'multi']) {
  const el = document.getElementById(id);
  el.addEventListener('input', () => window.log.push({ id, type: 'input', trusted: event.isTrusted }));
  el.addEventListener('change', () => window.log.push({
    id, type: 'change', trusted: event.isTrusted, count: el.files.length,
    names: Array.from(el.files).map(f => f.name), sizes: Array.from(el.files).map(f => f.size)
  }));
}
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

async fn eval(b: &CdpBackend, t: &str, js: &str) -> Value {
    b.eval(t, js).await.expect("eval")["result"].clone()
}

async fn upload(m: &BrowserModule, t: &str, query: &str, paths: &[&std::path::Path]) -> Envelope {
    let paths: Vec<String> = paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    m.call(
        "browser_upload",
        json!({ "target_id": t, "query": query, "paths": paths }),
        &ctx(),
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_set_through_a_label_reaches_the_page_as_a_trusted_change() {
    let Some((b, t)) = tab().await else {
        return;
    };
    let (base, stop) = serve().await;
    b.navigate(&t, "goto", Some(&format!("{base}/")))
        .await
        .expect("goto");

    let dir = std::env::temp_dir().join(format!("agentctl-upload-live-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cv = dir.join("cv.txt");
    std::fs::write(&cv, b"curriculum vitae").unwrap();
    let cv2 = dir.join("letter.txt");
    std::fs::write(&cv2, b"cover letter!").unwrap();
    let real = std::fs::canonicalize(&dir).unwrap();
    let resolver: mcp_browser::UploadResolver = Arc::new(move |p: &str| {
        let c = std::fs::canonicalize(p).map_err(|e| e.to_string())?;
        if c.starts_with(&real) {
            Ok(c)
        } else {
            Err(format!("path '{p}' resolves outside the allowed roots"))
        }
    });
    let m = BrowserModule::new(b.clone()).with_upload_resolver(resolver);

    // Targeting the label: the input is hidden and never clicked.
    let r = upload(&m, &t, "#pick", &[&cv]).await;
    assert!(r.ok, "{r:?}");
    let data = r.data.expect("data");
    assert_eq!(data["count"], 1);
    assert_eq!(data["files"][0]["name"], "cv.txt");
    assert_eq!(data["files"][0]["bytes"], 16);
    assert_eq!(data["input_multiple"], false);

    common::wait_until("the change event", Duration::from_secs(5), || async {
        eval(&b, &t, "window.log.some(e => e.type === 'change')").await == json!(true)
    })
    .await;
    let log = eval(&b, &t, "window.log").await;
    let change = log
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "change")
        .expect("change recorded");
    assert_eq!(change["id"], "single");
    assert_eq!(change["trusted"], true, "{log}");
    assert_eq!(change["count"], 1);
    assert_eq!(change["names"][0], "cv.txt");
    assert_eq!(change["sizes"][0], 16);
    let input = log
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "input")
        .expect("input recorded");
    assert_eq!(input["trusted"], true);

    // Two files into an input without `multiple`: refused, nothing changes.
    let before = eval(&b, &t, "window.log.length").await;
    let r = upload(&m, &t, "#single", &[&cv, &cv2]).await;
    assert!(!r.ok, "{r:?}");
    assert!(
        r.error.as_ref().unwrap().message.contains("multiple"),
        "{r:?}"
    );
    assert_eq!(eval(&b, &t, "window.log.length").await, before);

    // The same two files into the `multiple` input.
    let r = upload(&m, &t, "#multi", &[&cv, &cv2]).await;
    assert!(r.ok, "{r:?}");
    assert_eq!(r.data.as_ref().unwrap()["count"], 2);
    assert_eq!(r.data.as_ref().unwrap()["input_multiple"], true);
    common::wait_until("the multi change", Duration::from_secs(5), || async {
        eval(&b, &t, "document.getElementById('multi').files.length").await == json!(2)
    })
    .await;
    assert_eq!(
        eval(
            &b,
            &t,
            "Array.from(document.getElementById('multi').files).map(f => f.name).join(',')"
        )
        .await,
        "cv.txt,letter.txt"
    );

    // Something that is not (and does not hold) a file input says so.
    let r = upload(&m, &t, "#btn", &[&cv]).await;
    assert!(!r.ok);
    let msg = r.error.unwrap().message;
    assert!(
        msg.contains("<button>") && msg.contains("input[type=file]"),
        "{msg}"
    );

    // A path outside the resolver's root never reaches the page.
    let outside = std::env::temp_dir().join("agentctl-upload-outside.txt");
    std::fs::write(&outside, b"x").unwrap();
    let before = eval(&b, &t, "document.getElementById('single').files.length").await;
    let r = upload(&m, &t, "#single", &[&outside]).await;
    assert!(!r.ok);
    assert_eq!(
        eval(&b, &t, "document.getElementById('single').files.length").await,
        before
    );
    let _ = std::fs::remove_file(&outside);

    let _ = std::fs::remove_dir_all(&dir);
    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}
