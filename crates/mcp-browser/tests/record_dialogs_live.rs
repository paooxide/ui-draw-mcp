//! Live tests (real Chrome) for JavaScript dialogs while `browser_record` runs.
//!
//! The headless tests need only Chrome. The headed one (`human` default) also
//! needs `AGENTCTL_LIVE_GUI=1`, a display, and a way to press a key at the
//! native dialog (`osascript` with accessibility access on macOS, `xdotool` on
//! Linux); without those it skips and says so.
//!
//! Skipped when `AGENTCTL_SKIP_LIVE` is set or no Chrome binary is found.
//! CDP ports 9511-9513.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use mcp_browser::{
    BrowserBackend, BrowserModule, CdpBackend, FlowStore, Locator, NavPolicy, CHROME_BINS,
};
use mcp_types::{CallCtx, CancelToken, ToolModule};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn skip_live() -> bool {
    std::env::var_os("AGENTCTL_SKIP_LIVE").is_some_and(|v| v != "0")
}

fn have_chrome() -> bool {
    !skip_live() && CHROME_BINS.iter().any(|p| std::path::Path::new(p).exists())
}

async fn tab(port: u64, headless: bool) -> Option<(Arc<CdpBackend>, String)> {
    if !have_chrome() {
        return None;
    }
    let b = CdpBackend::new(NavPolicy::new(&[], true));
    b.connect(None, Some(json!({ "headless": headless, "port": port })))
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

/// `/` asks for confirmation from a timer (so no automation session is
/// attached when the dialog opens, only the recorder); `/sync` asks from the
/// click handler itself. `/done?r=...` is a beacon the page sends once the
/// confirm returned, and is logged.
async fn serve(log: Arc<Mutex<Vec<String>>>) -> (String, tokio::sync::oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut rx => break,
                res = listener.accept() => {
                    let Ok((mut stream, _)) = res else { continue };
                    let log = log.clone();
                    tokio::spawn(async move {
                        let mut buf = [0u8; 2048];
                        let n = stream.read(&mut buf).await.unwrap_or(0);
                        let req = String::from_utf8_lossy(&buf[..n]).to_string();
                        let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                        let body = if path.starts_with("/done") {
                            log.lock().unwrap().push(path.clone());
                            String::new()
                        } else if path.starts_with("/sync") {
                            r##"<!doctype html><body>
<button id="ask" onclick="var r = confirm('Delete everything?'); document.getElementById('o').textContent = 'r:' + r;">Ask</button>
<div id="o"></div></body>"##.to_string()
                        } else {
                            r##"<!doctype html><body>
<button id="ask" onclick="setTimeout(function () { var r = confirm('Delete everything?'); document.getElementById('o').textContent = 'r:' + r; fetch('/done?r=' + r); }, 300);">Ask</button>
<div id="o"></div></body>"##.to_string()
                        };
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
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

async fn js(b: &CdpBackend, target: &str, expr: &str) -> Value {
    let env = b.eval(target, expr).await.expect("eval");
    env.get("result")
        .cloned()
        .expect("eval envelope has result")
}

async fn click_ask(b: &CdpBackend, t: &str) {
    b.act(
        t,
        Locator::Selector {
            by: "css",
            query: "#ask",
            within: None,
            text: None,
            index: None,
        },
        "click",
        None,
    )
    .await
    .expect("click");
}

/// Index of the first step with this `op`.
fn pos(steps: &[Value], op: &str) -> usize {
    steps
        .iter()
        .position(|s| s["op"] == op)
        .unwrap_or_else(|| panic!("no {op} step in {steps:?}"))
}

/// With `dialogs: "accept"` the recorder answers the page's confirm "yes"
/// itself; the saved flow carries an accept step before the click that raised
/// it and replays to the same outcome, while the same flow without that step
/// does not (the default is to dismiss).
#[tokio::test(flavor = "multi_thread")]
async fn recording_with_accept_answers_the_dialog_and_the_flow_reproduces_it() {
    let Some((b, t)) = tab(9511, true).await else {
        return;
    };
    let (base, stop) = serve(Arc::default()).await;
    let dir = std::env::temp_dir().join(format!("agentctl-dlg-acc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::create_dir_all(&dir);
    let m = BrowserModule::new(b.clone()).with_flow_store(FlowStore::new(
        dir.join("flows.json"),
        10,
        100,
    ));
    b.navigate(&t, "goto", Some(&format!("{base}/")))
        .await
        .expect("navigate");

    let start = m
        .call(
            "browser_record",
            json!({ "target_id": t, "action": "start", "dialogs": "accept" }),
            &ctx(),
        )
        .await;
    assert!(start.ok, "{start:?}");
    assert_eq!(start.data.as_ref().unwrap()["dialogs"], "accept");

    click_ask(&b, &t).await;
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert_eq!(
        js(&b, &t, "document.getElementById('o').textContent").await,
        "r:true",
        "the recorder must have accepted the confirm"
    );

    let stopped = m
        .call(
            "browser_record",
            json!({ "target_id": t, "action": "stop", "name": "ask-accept" }),
            &ctx(),
        )
        .await;
    assert!(stopped.ok, "{stopped:?}");
    let steps: Vec<Value> = stopped.data.as_ref().unwrap()["flow"]["steps"]
        .as_array()
        .expect("steps")
        .clone();
    let d = &steps[pos(&steps, "dialog")];
    assert_eq!(d["policy"], "accept", "{steps:?}");
    assert_eq!(d["type"], "confirm");
    assert!(
        pos(&steps, "dialog") < pos(&steps, "act"),
        "the dialog step must come before the click that raised it: {steps:?}"
    );

    // Replay against the click-handler variant of the page (same selectors;
    // only the starting URL differs): the recorded dialog step is what makes
    // the confirm come out "yes".
    let on_sync: Vec<Value> = steps
        .iter()
        .map(|s| {
            let mut s = s.clone();
            if s["op"] == "navigate" {
                s["url"] = json!(format!("{base}/sync"));
            }
            s
        })
        .collect();
    let saved = m
        .call(
            "browser_flow",
            json!({ "action": "save", "name": "ask-sync", "steps": on_sync }),
            &ctx(),
        )
        .await;
    assert!(saved.ok, "{saved:?}");
    let run = |name: &'static str| {
        let (m, t) = (&m, &t);
        async move {
            m.call(
                "browser_flow",
                json!({ "action": "run", "name": name, "target_id": t }),
                &ctx(),
            )
            .await
        }
    };
    let r = run("ask-sync").await;
    assert!(r.ok, "replay: {r:?}");
    assert_eq!(
        js(&b, &t, "document.getElementById('o').textContent").await,
        "r:true",
        "replay must reproduce the accepted confirm"
    );

    // Control: without the dialog step the same flow dismisses.
    let without: Vec<Value> = on_sync
        .iter()
        .filter(|s| s["op"] != "dialog")
        .cloned()
        .collect();
    let saved = m
        .call(
            "browser_flow",
            json!({ "action": "save", "name": "ask-bare", "steps": without }),
            &ctx(),
        )
        .await;
    assert!(saved.ok, "{saved:?}");
    // Reset the tab's policy (the replay above left it on accept).
    let _ = b
        .dialog(&t, Some(mcp_browser::DialogPolicy::Dismiss))
        .await
        .expect("reset policy");
    let r = run("ask-bare").await;
    assert!(r.ok, "bare replay: {r:?}");
    assert_eq!(
        js(&b, &t, "document.getElementById('o').textContent").await,
        "r:false",
        "without the step the confirm is dismissed"
    );

    let _ = stop.send(());
    let _ = std::fs::remove_dir_all(&dir);
    let _ = b.disconnect(1, true).await;
}

/// A headless browser has nobody to answer: the default is to dismiss (and the
/// answer is reported), `human` is refused instead of hanging the tab, and an
/// unknown value is an argument error.
#[tokio::test(flavor = "multi_thread")]
async fn headless_recording_defaults_to_dismiss_and_refuses_human() {
    let Some((b, t)) = tab(9513, true).await else {
        return;
    };
    let (base, stop) = serve(Arc::default()).await;
    let m = BrowserModule::new(b.clone());
    b.navigate(&t, "goto", Some(&format!("{base}/")))
        .await
        .expect("navigate");

    let human = m
        .call(
            "browser_record",
            json!({ "target_id": t, "action": "start", "dialogs": "human" }),
            &ctx(),
        )
        .await;
    assert!(!human.ok, "human in a headless browser must be refused");
    let msg = human.error.as_ref().unwrap().message.clone();
    assert!(msg.contains("headless"), "{msg}");
    let st = m
        .call(
            "browser_record",
            json!({ "target_id": t, "action": "status" }),
            &ctx(),
        )
        .await;
    assert_eq!(st.data.as_ref().unwrap()["recording"], false, "{st:?}");

    let bogus = m
        .call(
            "browser_record",
            json!({ "target_id": t, "action": "start", "dialogs": "maybe" }),
            &ctx(),
        )
        .await;
    assert!(!bogus.ok);

    let start = m
        .call(
            "browser_record",
            json!({ "target_id": t, "action": "start" }),
            &ctx(),
        )
        .await;
    assert!(start.ok, "{start:?}");
    assert_eq!(start.data.as_ref().unwrap()["dialogs"], "dismiss");
    click_ask(&b, &t).await;
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert_eq!(
        js(&b, &t, "document.getElementById('o').textContent").await,
        "r:false"
    );
    let res = b.observe_stop(&t, "true").await.expect("stop");
    let steps: Vec<Value> = res["events"]
        .as_array()
        .expect("events")
        .iter()
        .filter(|e| e["kind"] == "dialog")
        .cloned()
        .collect();
    assert_eq!(steps.len(), 1, "{res}");
    assert_eq!(steps[0]["value"], "dismiss");
    assert_eq!(steps[0]["text"], "confirm");

    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

/// Press Return (accept) at the native dialog, as a person would. `None` when
/// this machine cannot synthesize the key (no accessibility access, no tool).
fn press_return_at_dialog() -> Option<()> {
    let out = if cfg!(target_os = "macos") {
        std::process::Command::new("osascript")
            .args([
                "-e",
                "tell application \"Google Chrome\" to activate",
                "-e",
                "delay 0.5",
                "-e",
                "tell application \"System Events\" to key code 36",
            ])
            .output()
            .ok()?
    } else {
        std::process::Command::new("xdotool")
            .args(["key", "Return"])
            .output()
            .ok()?
    };
    out.status.success().then_some(())
}

/// In a visible browser the default is `human`: the recorder must not answer,
/// the native dialog stays up for the person, and what they choose becomes a
/// dialog step. Gated on `AGENTCTL_LIVE_GUI=1` because it opens a window and
/// presses a key at it.
#[tokio::test(flavor = "multi_thread")]
async fn headed_recording_leaves_the_dialog_to_the_person_and_records_their_answer() {
    if std::env::var_os("AGENTCTL_LIVE_GUI").map_or(true, |v| v != "1") {
        eprintln!("skipping: set AGENTCTL_LIVE_GUI=1 to run the headed dialog test");
        return;
    }
    let Some((b, t)) = tab(9512, false).await else {
        return;
    };
    let log = Arc::new(Mutex::new(Vec::new()));
    let (base, stop) = serve(log.clone()).await;
    let m = BrowserModule::new(b.clone());
    b.navigate(&t, "goto", Some(&format!("{base}/")))
        .await
        .expect("navigate");

    let start = m
        .call(
            "browser_record",
            json!({ "target_id": t, "action": "start" }),
            &ctx(),
        )
        .await;
    assert!(start.ok, "{start:?}");
    assert_eq!(
        start.data.as_ref().unwrap()["dialogs"],
        "human",
        "a visible browser defaults to the person answering"
    );

    click_ask(&b, &t).await;
    tokio::time::sleep(Duration::from_millis(2_000)).await;
    assert!(
        log.lock().unwrap().is_empty(),
        "the recorder answered a dialog that was the person's to answer: {:?}",
        log.lock().unwrap()
    );

    if press_return_at_dialog().is_none() {
        eprintln!(
            "skipping the rest: could not press a key at the native dialog (needs accessibility access for osascript, or xdotool)"
        );
        let _ = stop.send(());
        let _ = b.disconnect(1, true).await;
        return;
    }
    let mut done = false;
    for _ in 0..50 {
        if log.lock().unwrap().iter().any(|p| p.contains("r=true")) {
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        done,
        "the page never saw the person accept: {:?}",
        log.lock()
    );

    let res = b.observe_stop(&t, "true").await.expect("stop");
    let dialogs = res["dialogs"].as_array().expect("dialogs");
    assert!(
        dialogs
            .iter()
            .any(|d| d["by"] == "person" && d["answered"] == "accepted"),
        "{res}"
    );
    assert!(
        res["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "dialog" && e["value"] == "accept"),
        "{res}"
    );

    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}
