//! The browser engine under `foreground = "never"` and `"polite"`, against a
//! real headless Chrome.
//!
//! What a flow sends to the browser is read off the wire: `cdp::wire_log`
//! records every command this process sends to a given browser port, so
//! "no `Page.bringToFront`" is a statement about the real session and not
//! about a mock. Each test also runs the same step under `allow`, where the
//! raising command *is* sent, so an absence under `never` cannot be the
//! recorder failing to see it.
//!
//! Headless has no window, so what these show is which commands the engine
//! issues and that a normal flow, including a page that depends on focus,
//! works without them. That a visible browser actually stays behind other
//! windows is measured on a real screen in
//! `agentctl/tests/live_browser_headed_never.rs`.
//!
//! Skipped when `AGENTCTL_SKIP_LIVE` is set or no Chrome binary is found. One
//! Chrome at a time: the tests share a lock.

use std::sync::Arc;

use mcp_types::{CallCtx, CancelToken, Foreground, ToolModule};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::cdp::wire_log;
use crate::{BrowserBackend, BrowserError, BrowserModule, CdpBackend, NavPolicy, CHROME_BINS};

const FOCUS_PAGE: &str = include_str!("../../../docs/fixtures/focus_probe.html");

static ONE_CHROME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn have_chrome() -> bool {
    !std::env::var_os("AGENTCTL_SKIP_LIVE").is_some_and(|v| v != "0")
        && CHROME_BINS.iter().any(|p| std::path::Path::new(p).exists())
}

fn ctx() -> CallCtx {
    CallCtx::new("test", CancelToken::new())
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
                        let _ = stream.read(&mut buf).await;
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                             Cache-Control: no-store\r\nContent-Length: {}\r\n\
                             Connection: close\r\n\r\n{}",
                            FOCUS_PAGE.len(),
                            FOCUS_PAGE
                        );
                        let _ = stream.write_all(resp.as_bytes()).await;
                        let _ = stream.flush().await;
                    });
                }
            }
        }
    });
    (format!("http://127.0.0.1:{port}/"), tx)
}

struct Rig {
    b: Arc<CdpBackend>,
    id: u32,
    port: u16,
    connected: Value,
}

async fn start(fg: Foreground) -> Rig {
    let b = Arc::new(CdpBackend::new(NavPolicy::new(&[], true)).with_foreground(fg));
    let out = b
        .connect(None, Some(json!({ "headless": true, "port": 0 })))
        .await
        .expect("a headless launch is allowed under every setting");
    Rig {
        id: out["browser_id"].as_u64().expect("browser_id") as u32,
        port: out["port"].as_u64().expect("port") as u16,
        b,
        connected: out,
    }
}

impl Rig {
    fn methods(&self) -> Vec<String> {
        wire_log::sent_to(self.port)
            .into_iter()
            .map(|(m, _)| m)
            .collect()
    }

    async fn first_tab(&self) -> String {
        let tabs = self
            .b
            .tabs(self.id, "list", None, None)
            .await
            .expect("tabs");
        tabs["tabs"][0]["target_id"]
            .as_str()
            .expect("a tab")
            .to_string()
    }

    async fn stop(self) {
        let _ = self.b.disconnect(self.id, true).await;
    }
}

const RAISING: [&str; 2] = ["Page.bringToFront", "Target.activateTarget"];

#[tokio::test(flavor = "multi_thread")]
async fn connecting_brings_the_page_forward_only_under_allow() {
    if !have_chrome() {
        return;
    }
    let _one = ONE_CHROME.lock().await;
    let rig = start(Foreground::Allow).await;
    assert_eq!(
        rig.connected["foregrounded"],
        json!(true),
        "{}",
        rig.connected
    );
    assert!(
        rig.methods().iter().any(|m| m == "Page.bringToFront"),
        "the recorder must see the raising command under allow: {:?}",
        rig.methods()
    );
    rig.stop().await;

    for fg in [Foreground::Never, Foreground::Polite] {
        let rig = start(fg).await;
        assert_eq!(rig.connected["foregrounded"], json!(false), "{fg:?}");
        let tab = rig.first_tab().await;
        rig.b
            .navigate(&tab, "goto", Some("about:blank"))
            .await
            .expect("a normal call still works");
        let sent = rig.methods();
        assert!(!sent.is_empty(), "{fg:?}: nothing was recorded");
        for m in RAISING {
            assert!(!sent.iter().any(|s| s == m), "{fg:?} sent {m}: {sent:?}");
        }
        rig.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_tab_is_created_in_the_background_under_never_and_polite() {
    if !have_chrome() {
        return;
    }
    let _one = ONE_CHROME.lock().await;
    let rig = start(Foreground::Allow).await;
    let opened = rig
        .b
        .tabs(rig.id, "open", None, Some("about:blank"))
        .await
        .expect("open");
    assert!(opened["target_id"].is_string(), "{opened}");
    assert!(
        !rig.methods().iter().any(|m| m == "Target.createTarget"),
        "allow opens tabs through /json/new, as it always did"
    );
    rig.stop().await;

    for fg in [Foreground::Never, Foreground::Polite] {
        let rig = start(fg).await;
        let opened = rig
            .b
            .tabs(rig.id, "open", None, Some("about:blank"))
            .await
            .expect("open");
        let id = opened["target_id"].as_str().expect("target_id").to_string();
        let created: Vec<Value> = wire_log::sent_to(rig.port)
            .into_iter()
            .filter(|(m, _)| m == "Target.createTarget")
            .map(|(_, p)| p)
            .collect();
        assert_eq!(created.len(), 1, "{fg:?}: {created:?}");
        assert_eq!(created[0]["background"], json!(true), "{fg:?}");
        // The tab is real: listed, and a page can be driven in it.
        let listed = rig.b.tabs(rig.id, "list", None, None).await.expect("list");
        assert!(
            listed["tabs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["target_id"] == json!(id)),
            "{fg:?}: {listed}"
        );
        let two = rig.b.eval(&id, "1 + 1").await.expect("eval in the new tab");
        assert_eq!(two["result"], json!(2));
        for m in RAISING {
            assert!(!rig.methods().iter().any(|s| s == m), "{fg:?} sent {m}");
        }
        rig.stop().await;
    }
}

/// A headless browser has no window to raise, so activating a tab goes
/// ahead; what a visible one does is decided by the classification test.
#[tokio::test(flavor = "multi_thread")]
async fn activating_a_tab_of_a_headless_browser_is_not_refused() {
    if !have_chrome() {
        return;
    }
    let _one = ONE_CHROME.lock().await;
    let rig = start(Foreground::Never).await;
    let tab = rig.first_tab().await;
    let r = rig
        .b
        .tabs(rig.id, "activate", Some(&tab), None)
        .await
        .expect("headless activation is harmless");
    assert_eq!(r["activated"], json!(tab));
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_visible_launch_is_refused_before_anything_starts() {
    for fg in [Foreground::Never, Foreground::Polite] {
        let b = CdpBackend::new(NavPolicy::new(&[], true)).with_foreground(fg);
        for spec in [
            json!({ "headless": false }),
            json!({}),
            json!({ "browser": "safari" }),
        ] {
            let e = b.connect(None, Some(spec.clone())).await.unwrap_err();
            let BrowserError::Refused(m) = e else {
                panic!("{fg:?} {spec}: not a refusal: {e:?}");
            };
            assert!(m.contains(fg.as_str()), "{m}");
            assert!(m.contains("headless: true"), "{m}");
            assert!(m.contains("nothing was done"), "{m}");
        }
        assert!(b.browser_ids().await.is_empty(), "something was started");
    }
}

/// The same page, driven the same way, under every setting: navigate, focus a
/// field, type, click, move focus. Its focus-dependent behaviour must be
/// that of a focused page, without a window being raised.
#[tokio::test(flavor = "multi_thread")]
async fn a_focus_dependent_page_works_without_the_window_in_front() {
    if !have_chrome() {
        return;
    }
    let _one = ONE_CHROME.lock().await;
    let (url, stop_server) = serve().await;
    for fg in [Foreground::Allow, Foreground::Never, Foreground::Polite] {
        let rig = start(fg).await;
        let tab = rig.first_tab().await;
        let m = BrowserModule::new(rig.b.clone());
        rig.b
            .navigate(&tab, "goto", Some(&url))
            .await
            .expect("goto");
        let act = |query: &'static str, action: &'static str, value: Option<&'static str>| {
            let m = &m;
            let tab = tab.clone();
            async move {
                let mut args = json!({ "target_id": tab, "query": query, "action": action });
                if let Some(v) = value {
                    args["value"] = json!(v);
                }
                let env = m.call("browser_act", args, &ctx()).await;
                assert!(env.ok, "{fg:?} {action} {query}: {env:?}");
            }
        };
        let seen = |rig: &Rig| {
            let b = rig.b.clone();
            let tab = tab.clone();
            async move {
                let v = b
                    .eval(&tab, "document.getElementById('log').textContent")
                    .await
                    .expect("eval");
                serde_json::from_str::<Value>(v["result"].as_str().expect("log text"))
                    .expect("log is JSON")
            }
        };

        act("#a", "focus", None).await;
        let s = seen(&rig).await;
        assert_eq!(s["a_focus"], json!(1), "{fg:?}: {s}");
        assert_eq!(s["active"], json!("a"), "{fg:?}: {s}");
        assert_eq!(s["a_matches_focus"], json!(true), "{fg:?}: {s}");
        assert_eq!(s["a_outline"], json!("rgb(0, 128, 0)"), "{fg:?}: {s}");
        assert_eq!(s["has_focus_now"], json!(true), "{fg:?}: {s}");

        act("#a", "type", Some("hello")).await;
        assert_eq!(seen(&rig).await["typed"], json!("hello"), "{fg:?}");

        act("#go", "click", None).await;
        let s = seen(&rig).await;
        assert_eq!(s["clicks"], json!(1), "{fg:?}: {s}");

        act("#b", "focus", None).await;
        let s = seen(&rig).await;
        assert_eq!(s["a_blur"], json!(1), "{fg:?}: {s}");
        assert_eq!(s["b_focus"], json!(1), "{fg:?}: {s}");
        assert_eq!(s["active"], json!("b"), "{fg:?}: {s}");
        assert_eq!(s["a_outline"], json!("rgb(200, 200, 200)"), "{fg:?}: {s}");
        assert_eq!(s["has_focus_now"], json!(true), "{fg:?}: {s}");

        let sent = rig.methods();
        let focus_emulated = sent
            .iter()
            .any(|m| m == "Emulation.setFocusEmulationEnabled");
        assert!(
            focus_emulated,
            "{fg:?}: sessions must emulate focus: {sent:?}"
        );
        if fg != Foreground::Allow {
            for m in RAISING {
                assert!(!sent.iter().any(|s| s == m), "{fg:?} sent {m}: {sent:?}");
            }
        }
        rig.stop().await;
    }
    let _ = stop_server.send(());
}

/// A branch lives in a context of its own, which in a visible browser is a
/// window: under `never` the tab is created in the background.
#[tokio::test(flavor = "multi_thread")]
async fn a_branch_tab_is_created_in_the_background_under_never() {
    if !have_chrome() {
        return;
    }
    let _one = ONE_CHROME.lock().await;
    let rig = start(Foreground::Never).await;
    let tab = rig.first_tab().await;
    rig.b.branch_create(&tab, "probe").await.expect("branch");
    let created: Vec<Value> = wire_log::sent_to(rig.port)
        .into_iter()
        .filter(|(m, _)| m == "Target.createTarget")
        .map(|(_, p)| p)
        .collect();
    assert_eq!(created.len(), 1, "{created:?}");
    assert_eq!(created[0]["background"], json!(true));
    assert!(created[0]["browserContextId"].is_string());
    let _ = rig.b.branch_discard("probe").await;
    rig.stop().await;
}

/// A recording brings its page to the front when it starts, under `allow`;
/// under the other settings it does not, and the page is still rendered and
/// captured (the holder session keeps it believing it has focus).
#[tokio::test(flavor = "multi_thread")]
async fn a_screencast_does_not_bring_its_page_forward_under_never() {
    if !have_chrome() {
        return;
    }
    let _one = ONE_CHROME.lock().await;
    let media = std::env::temp_dir().join(format!("agentctl-fg-cast-{}", std::process::id()));
    for fg in [Foreground::Allow, Foreground::Never] {
        let rig = start(fg).await;
        let tab = rig.first_tab().await;
        rig.b
            .navigate(&tab, "goto", Some("about:blank"))
            .await
            .expect("goto");
        let opts = crate::ScreencastOpts::from_args(Some(5), Some(50), Some(5));
        rig.b
            .screencast_start(&tab, &media, opts)
            .await
            .unwrap_or_else(|e| panic!("{fg:?}: {e:?}"));
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        let stopped = rig.b.screencast_stop(Some(&tab), None, true).await;
        let sent = rig.methods();
        assert_eq!(
            sent.iter().any(|m| m == "Page.bringToFront"),
            fg == Foreground::Allow,
            "{fg:?}: {sent:?}"
        );
        assert!(
            sent.iter().any(|m| m == "Page.captureScreenshot"),
            "{fg:?}: frames were captured: {sent:?} {stopped:?}"
        );
        rig.stop().await;
    }
    let _ = std::fs::remove_dir_all(&media);
}
