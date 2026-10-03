//! Live tests (real headless Chrome): a checkpoint rollback loads the page
//! from the network, not from Chrome's HTTP cache.
//!
//! The fixture page is served with `Cache-Control: max-age=3600`, so Chrome
//! would happily serve it from disk for an hour, whatever the server now says
//! (or whether it is still there).
//!
//! Skipped when `AGENTCTL_SKIP_LIVE` is set or no Chrome binary is found.

mod common;

use std::sync::{Arc, Mutex};

use mcp_browser::{BrowserBackend, BrowserError, CdpBackend, NavPolicy, CHROME_BINS};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn skip_live() -> bool {
    std::env::var_os("AGENTCTL_SKIP_LIVE").is_some_and(|v| v != "0")
}

fn have_chrome() -> bool {
    !skip_live() && CHROME_BINS.iter().any(|p| std::path::Path::new(p).exists())
}

async fn tab() -> Option<(CdpBackend, String)> {
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
    Some((b, target))
}

/// What the fixture server says `/z` is, and how many times it was asked.
#[derive(Default)]
struct Site {
    z_body: Mutex<String>,
    z_hits: Mutex<u32>,
}

/// `/z` is cacheable for an hour; `/b` is never cached. Stopping the server
/// (the returned sender) closes the listener so the port refuses connections.
async fn serve(site: Arc<Site>) -> (String, tokio::sync::oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
    // Tells every connection task to let go of its socket when the server stops.
    let (down_tx, down_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut rx => {
                    let _ = down_tx.send(true);
                    break;
                }
                res = listener.accept() => {
                    let Ok((mut stream, _)) = res else { continue };
                    let site = site.clone();
                    let mut down = down_rx.clone();
                    tokio::spawn(async move {
                        let mut buf = [0u8; 2048];
                        // Idle speculative sockets are closed the moment the
                        // server stops, or Chrome could reuse one and the
                        // server would never really be "down".
                        let n = tokio::select! {
                            _ = down.changed() => 0,
                            r = stream.read(&mut buf) => r.unwrap_or(0),
                        };
                        if n == 0 {
                            return;
                        }
                        let req = String::from_utf8_lossy(&buf[..n]).to_string();
                        let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                        let (cache, text) = if path == "/z" {
                            *site.z_hits.lock().unwrap() += 1;
                            ("max-age=3600", site.z_body.lock().unwrap().clone())
                        } else {
                            ("no-store", "page b".to_string())
                        };
                        let body = format!(
                            "<!doctype html><body><h1 id=\"which\">{text}</h1></body>"
                        );
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: {cache}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
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

async fn which(b: &CdpBackend, t: &str) -> Value {
    let env = b
        .eval(t, "document.getElementById('which').textContent")
        .await
        .expect("eval");
    env["result"].clone()
}

fn err_text(e: &BrowserError) -> String {
    match e {
        BrowserError::PermissionDenied(m)
        | BrowserError::NotFound(m)
        | BrowserError::Unsupported(m)
        | BrowserError::Timeout(m)
        | BrowserError::Failed(m) => m.clone(),
    }
}

/// The fixture really is cached: going to it again does not reach the server.
/// If this fails the setup proves nothing, so the tests check it first.
async fn assert_cache_is_in_play(b: &CdpBackend, t: &str, site: &Site, z: &str, b_url: &str) {
    b.navigate(t, "goto", Some(b_url)).await.expect("to b");
    let before = *site.z_hits.lock().unwrap();
    b.navigate(t, "goto", Some(z)).await.expect("to z again");
    assert_eq!(
        *site.z_hits.lock().unwrap(),
        before,
        "Chrome re-fetched the cacheable page: the fixture is not exercising the cache"
    );
}

/// The server holding the checkpointed page goes away. The page is still in
/// Chrome's cache, but a rollback that served it would report success for a
/// page nobody fetched; it must say it could not reach the page.
#[tokio::test(flavor = "multi_thread")]
async fn rollback_does_not_serve_a_cached_page_when_the_server_is_down() {
    let Some((b, t)) = tab().await else {
        return;
    };
    let site = Arc::new(Site::default());
    *site.z_body.lock().unwrap() = "page z".into();
    let (base, stop) = serve(site.clone()).await;
    let (z, other) = (format!("{base}/z"), format!("{base}/b"));

    b.navigate(&t, "goto", Some(&z)).await.expect("to z");
    assert_eq!(which(&b, &t).await, "page z");
    b.checkpoint_save(&t, Some("z")).await.expect("save");
    assert_cache_is_in_play(&b, &t, &site, &z, &other).await;
    b.navigate(&t, "goto", Some(&other)).await.expect("to b");

    let _ = stop.send(());
    let host = base.trim_start_matches("http://").to_string();
    common::wait_until(
        "the fixture server to stop accepting",
        std::time::Duration::from_secs(5),
        || async { TcpStream::connect(host.as_str()).await.is_err() },
    )
    .await;
    assert!(
        TcpStream::connect(host.as_str()).await.is_err(),
        "fixture server should be down"
    );

    let res = b.checkpoint_rollback(&t, Some("z")).await;
    let err = res
        .expect_err("rollback to a page the server cannot serve must fail, not succeed from cache");
    assert!(
        err_text(&err).to_lowercase().contains("navigate"),
        "{}",
        err_text(&err)
    );
    let _ = b.disconnect(1, true).await;
}

/// The server's page changed since the checkpoint. The rollback shows what the
/// server says now (it fetched), and says it bypassed the cache.
#[tokio::test(flavor = "multi_thread")]
async fn rollback_loads_the_servers_current_page_not_the_cached_one() {
    let Some((b, t)) = tab().await else {
        return;
    };
    let site = Arc::new(Site::default());
    *site.z_body.lock().unwrap() = "page z v1".into();
    let (base, stop) = serve(site.clone()).await;
    let (z, other) = (format!("{base}/z"), format!("{base}/b"));

    b.navigate(&t, "goto", Some(&z)).await.expect("to z");
    assert_eq!(which(&b, &t).await, "page z v1");
    b.checkpoint_save(&t, Some("z")).await.expect("save");
    assert_cache_is_in_play(&b, &t, &site, &z, &other).await;
    b.navigate(&t, "goto", Some(&other)).await.expect("to b");

    *site.z_body.lock().unwrap() = "page z v2".into();
    let hits_before = *site.z_hits.lock().unwrap();
    let rolled = b
        .checkpoint_rollback(&t, Some("z"))
        .await
        .expect("rollback");
    assert_eq!(rolled["navigated"], true, "{rolled}");
    assert_eq!(rolled["cache_bypassed"], true, "{rolled}");
    assert_eq!(
        which(&b, &t).await,
        "page z v2",
        "rollback showed a cached copy of the page"
    );
    assert!(
        *site.z_hits.lock().unwrap() > hits_before,
        "the rollback never reached the server"
    );

    // The cache switch was for the rollback's own load: going there again
    // normally is served from cache once more.
    b.navigate(&t, "goto", Some(&other)).await.expect("to b");
    let hits = *site.z_hits.lock().unwrap();
    b.navigate(&t, "goto", Some(&z)).await.expect("to z");
    assert_eq!(
        *site.z_hits.lock().unwrap(),
        hits,
        "the cache bypass leaked into later navigation"
    );

    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}
