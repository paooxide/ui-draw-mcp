//! Safari (WebKit) via `safaridriver`.
//!
//! The live test drives a real, visible Safari window, so it needs all of:
//! macOS with `safaridriver`, `safaridriver --enable` run once by a person
//! (it asks for an administrator password), `AGENTCTL_LIVE_SAFARI=1`, and no
//! `AGENTCTL_SKIP_LIVE`. If remote automation is not enabled the test asserts
//! the failure maps to `PermissionDenied` with the fix in the message, and
//! stops there, rather than pretending to have covered anything.
//!
//! The argument-validation tests need no Safari: they are refused before any
//! driver is started.

use mcp_browser::{
    is_safari_available, BrowserBackend, BrowserError, CdpBackend, DialogPolicy, Locator, NavPolicy,
};
use serde_json::json;
use std::io::{Read, Write};
use std::net::TcpListener;

fn live_safari() -> bool {
    let skip = std::env::var_os("AGENTCTL_SKIP_LIVE").is_some_and(|v| v != "0");
    !skip
        && std::env::var_os("AGENTCTL_LIVE_SAFARI").is_some_and(|v| v == "1")
        && is_safari_available()
}

const FIXTURE: &str = r#"<!doctype html><title>fixture</title>
<button id="btn" onclick="document.getElementById('out').textContent='clicked'">go</button>
<input id="in"><div id="out">idle</div>"#;

/// Serve `FIXTURE` on loopback, so the page passes the same navigation policy
/// a real URL does (`data:` and `file:` are refused by it).
fn serve_fixture() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { break };
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf);
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                FIXTURE.len(),
                FIXTURE
            );
            let _ = s.write_all(resp.as_bytes());
        }
    });
    port
}

fn sel(q: &str) -> Locator<'_> {
    Locator::Selector {
        by: "css",
        query: q,
        within: None,
        text: None,
        index: None,
    }
}

#[tokio::test]
async fn unknown_browser_and_bad_port_are_refused() {
    let b = CdpBackend::new(NavPolicy::default());
    let e = b
        .connect(None, Some(json!({ "browser": "firefox" })))
        .await
        .unwrap_err();
    assert!(matches!(e, BrowserError::Unsupported(_)), "{e:?}");

    // `url` is honoured only by Safari; a Chromium launch must not drop it.
    let e = b
        .connect(None, Some(json!({ "url": "https://example.com" })))
        .await
        .unwrap_err();
    assert!(matches!(e, BrowserError::Unsupported(_)), "{e:?}");

    if cfg!(target_os = "macos") && is_safari_available() {
        let e = b
            .connect(None, Some(json!({ "browser": "safari", "port": 70000 })))
            .await
            .unwrap_err();
        assert!(matches!(e, BrowserError::Failed(m) if m.contains("valid TCP port")));
    }
}

/// The first URL is judged before any driver starts, so a denied URL costs
/// nothing and nothing is left running.
#[tokio::test]
async fn safari_launch_url_goes_through_navigation_policy() {
    if !(cfg!(target_os = "macos") && is_safari_available()) {
        return;
    }
    let b = CdpBackend::new(NavPolicy::default());
    for url in [
        "file:///etc/hosts",
        "http://169.254.169.254/latest/meta-data",
    ] {
        let e = b
            .connect(None, Some(json!({ "browser": "safari", "url": url })))
            .await
            .unwrap_err();
        assert!(
            matches!(e, BrowserError::PermissionDenied(_)),
            "{url}: {e:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn safari_session_act_and_failure_reporting() {
    if !live_safari() {
        return;
    }
    let port = serve_fixture();
    let url = format!("http://127.0.0.1:{port}/");
    let b = CdpBackend::new(NavPolicy::new(&[], true));

    let c = match b
        .connect(None, Some(json!({ "browser": "safari", "url": url })))
        .await
    {
        Ok(c) => c,
        Err(BrowserError::PermissionDenied(m)) => {
            // Safari would not take an automation session (setting off, or a
            // prompt unanswered): assert the mapping and say so.
            assert!(m.contains("safaridriver --enable"), "{m}");
            assert!(m.contains("Allow Remote Automation"), "{m}");
            eprintln!(
                "safari live test: automation not enabled or not accepted; only the error path ran"
            );
            return;
        }
        Err(e) => panic!("connect failed: {e:?}"),
    };
    let id = c["browser_id"].as_u64().unwrap() as u32;
    let t = c["target_id"].as_str().unwrap().to_string();

    // Click and type both have to take effect: the page is the witness.
    b.act(&t, sel("#btn"), "click", None).await.unwrap();
    let out = b
        .eval(&t, "document.getElementById('out').textContent")
        .await
        .unwrap();
    assert_eq!(out["result"], "clicked");

    b.act(&t, sel("#in"), "type", Some("hello")).await.unwrap();
    let v = b
        .eval(&t, "document.getElementById('in').value")
        .await
        .unwrap();
    assert_eq!(v["result"], "hello");

    // A failing act must be an error, never a silent success.
    let e = b
        .act(&t, sel("#does-not-exist"), "click", None)
        .await
        .unwrap_err();
    assert!(matches!(e, BrowserError::NotFound(_)), "{e:?}");
    let e = b
        .fill_form(&t, &json!([{ "selector": "#nope", "value": "x" }]), None)
        .await
        .unwrap_err();
    assert!(matches!(e, BrowserError::Failed(_)), "{e:?}");
    b.fill_form(&t, &json!([{ "selector": "#in", "value": "filled" }]), None)
        .await
        .unwrap();

    // Navigation policy applies on every route.
    let e = b.navigate(&t, "goto", Some("file:///etc/hosts")).await;
    assert!(matches!(e, Err(BrowserError::PermissionDenied(_))));
    let e = b.tabs(id, "open", None, Some("file:///etc/hosts")).await;
    assert!(matches!(e, Err(BrowserError::PermissionDenied(_))));

    // Things WebDriver cannot do are refused, not reported as done.
    let e = b.set_viewport(&t, 800, 600, true, 1.0).await.unwrap_err();
    assert!(matches!(e, BrowserError::Unsupported(_)), "{e:?}");
    let e = b.set_viewport(&t, 0, 0, false, 1.0).await.unwrap_err();
    assert!(matches!(e, BrowserError::Unsupported(_)), "{e:?}");
    let e = b
        .dialog(&t, Some(DialogPolicy::Accept(None)))
        .await
        .unwrap_err();
    assert!(matches!(e, BrowserError::NotFound(_)), "{e:?}");

    // Element screenshot measures the element.
    let q = b.query(&t, "css", "#btn", false).await.unwrap();
    let r = q["matches"][0]["ref"].as_str().unwrap().to_string();
    let shot = b.screenshot(&t, Some(&r)).await.unwrap();
    assert!(shot.width > 0 && shot.height > 0);
    assert!(!shot.base64.is_empty());

    b.disconnect(id, true).await.unwrap();
}
