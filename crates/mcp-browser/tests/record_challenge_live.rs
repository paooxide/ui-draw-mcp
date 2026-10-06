//! Live tests (real headless Chrome) for the Ghost Mode recorder, the
//! challenge handshake, and `browser_wait network_idle`.
//!
//! Skipped when `AGENTCTL_SKIP_LIVE` is set or no Chrome binary is found.

use std::sync::Arc;

use mcp_browser::record::is_secret_field;
use mcp_browser::{
    BrowserBackend, BrowserModule, CdpBackend, ChallengeKind, ChallengeManager, FlowStore, Locator,
    NavPolicy, RecordManager, CHROME_BINS,
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

/// The page value of an eval (unwraps the real `{"result": v}` envelope).
async fn js(b: &CdpBackend, target: &str, expr: &str) -> Value {
    let env = b.eval(target, expr).await.expect("eval");
    env.get("result")
        .cloned()
        .expect("eval envelope has result")
}

fn ctx() -> CallCtx {
    CallCtx::new("test", CancelToken::new())
}

const FIXTURE: &str = r#"<!doctype html><html><body>
<input id="name" placeholder="name">
<input id="pw" type="password">
<input id="otp" autocomplete="one-time-code">
<form id="f" onsubmit="event.preventDefault(); document.getElementById('out2').textContent='submitted:'+document.getElementById('q').value;">
  <input id="q">
</form>
<button id="go" onclick="document.getElementById('out').textContent='hello '+document.getElementById('name').value">Go</button>
<div id="out"></div><div id="out2"></div>
</body></html>"#;

/// Stop recording and return the raw captured events (without `navigate`
/// ones) as `{"events": [...]}`, for tests that inspect what the page sent.
async fn recorded_events(b: &Arc<CdpBackend>, t: &str) -> Value {
    let events = RecordManager::stop_raw(b.as_ref(), t)
        .await
        .expect("stop recording");
    let kept: Vec<&mcp_browser::RawInteractionEvent> =
        events.iter().filter(|e| e.kind != "navigate").collect();
    json!({ "events": kept })
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

#[tokio::test(flavor = "multi_thread")]
async fn recorded_flow_round_trips_through_browser_flow_without_secrets() {
    let Some((b, t)) = tab().await else {
        return;
    };
    let (base, stop) = serve_html(FIXTURE.to_string()).await;
    b.navigate(&t, "goto", Some(&base)).await.expect("navigate");

    RecordManager::start(b.as_ref(), &t).await.expect("start");
    act(&b, &t, "#name", "type", Some("Ada")).await;
    act(&b, &t, "#pw", "type", Some("hunter2-secret")).await;
    act(&b, &t, "#otp", "type", Some("123456")).await;
    act(&b, &t, "#q", "type", Some("zed")).await;
    act(&b, &t, "#q", "press", Some("Enter")).await;
    act(&b, &t, "#go", "click", None).await;
    // Sanity: the driven page really did the work while recording.
    assert_eq!(
        js(&b, &t, "document.getElementById('out').textContent").await,
        "hello Ada"
    );
    assert_eq!(
        js(&b, &t, "document.getElementById('out2').textContent").await,
        "submitted:zed",
        "a real Enter key press must trigger implicit form submission"
    );

    let steps = RecordManager::stop(b.as_ref(), &t).await.expect("stop");
    let dump = serde_json::to_string(&steps).unwrap();
    assert!(!dump.contains("hunter2-secret"), "password leaked: {dump}");
    assert!(!dump.contains("123456"), "otp leaked: {dump}");
    assert!(!steps.is_empty(), "recording must yield steps: {dump}");

    let secret_steps: Vec<&Value> = steps.iter().filter(|s| s["secret"] == true).collect();
    assert_eq!(secret_steps.len(), 2, "pw and otp: {dump}");
    let mut refs: Vec<&str> = secret_steps
        .iter()
        .map(|s| s["secret_ref"].as_str().expect("secret_ref"))
        .collect();
    refs.sort_unstable();
    assert_eq!(refs, ["otp", "pw"], "{dump}");
    for s in &secret_steps {
        assert!(
            s.get("value").is_none(),
            "a secret step holds no value: {s}"
        );
    }
    assert!(
        steps
            .iter()
            .any(|s| s["action"] == "press" && s["value"] == "Enter"),
        "Enter keypress recorded: {dump}"
    );

    // Replay on a fresh load through browser_flow.
    let dir = std::env::temp_dir().join(format!("agentctl-rec-rt-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let store = FlowStore::new(dir.join("flows.json"), 10, 100);
    let m = BrowserModule::new(b.clone()).with_flow_store(store);
    let e = m
        .call(
            "browser_flow",
            json!({ "action": "save", "name": "rec", "steps": steps }),
            &ctx(),
        )
        .await;
    assert!(e.ok, "{e:?}");
    let run = |secrets: Value| {
        let (m, t) = (&m, &t);
        async move {
            m.call(
                "browser_flow",
                json!({ "action": "run", "name": "rec", "target_id": t, "secrets": secrets }),
                &ctx(),
            )
            .await
        }
    };

    // 1. No secrets: the run is refused before any step, naming the ref, and
    //    nothing is typed.
    b.navigate(&t, "goto", Some(&base)).await.expect("reload");
    let e = run(json!({})).await;
    assert!(!e.ok, "replay without secrets must fail: {e:?}");
    let msg = e.error.as_ref().expect("error").message.clone();
    assert!(msg.contains("'otp'") || msg.contains("'pw'"), "{msg}");
    assert_eq!(js(&b, &t, "document.getElementById('pw').value").await, "");
    assert_eq!(
        js(&b, &t, "document.getElementById('name').value").await,
        ""
    );

    // 2. Supplied at run time, the flow runs end to end.
    let e = run(json!({ "pw": "pw-supplied", "otp": "654321" })).await;
    assert!(e.ok, "replay should pass: {e:?}");
    assert_eq!(
        js(&b, &t, "document.getElementById('out').textContent").await,
        "hello Ada"
    );
    assert_eq!(
        js(&b, &t, "document.getElementById('out2').textContent").await,
        "submitted:zed"
    );
    assert_eq!(
        js(&b, &t, "document.getElementById('pw').value").await,
        "pw-supplied"
    );
    // Nothing the run returned, and nothing on disk, holds a secret.
    let returned = serde_json::to_string(&e).unwrap();
    let on_disk = std::fs::read_to_string(dir.join("flows.json")).unwrap();
    for leaked in ["pw-supplied", "654321", "hunter2-secret", "123456"] {
        assert!(!returned.contains(leaked), "{leaked} in the result");
        assert!(!on_disk.contains(leaked), "{leaked} in the flow store");
    }

    let _ = stop.send(());
    let _ = std::fs::remove_dir_all(&dir);
    let _ = b.disconnect(1, true).await;
}

/// A small two-page site: `/a` links to `/b` (which answers after a short
/// delay), and `/b` has a field and a button that reports what was typed.
async fn serve_site() -> (String, tokio::sync::oneshot::Sender<()>) {
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
                        let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                        let body = if path.starts_with("/b") {
                            // Fixture behaviour, not a readiness wait: page B is
                            // deliberately slow so the recorder sees a late load.
                            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                            r#"<!doctype html><body><h1>page b</h1>
<input id="b-in">
<button id="b-go" onclick="document.getElementById('out').textContent='B:'+document.getElementById('b-in').value">Go</button>
<div id="out"></div></body>"#
                        } else {
                            r#"<!doctype html><body><h1>page a</h1>
            <a id="to-b" href="/b">to B</a></body>"#
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

/// The recorder lives through navigation. Click a link on page A, type on page
/// B after it loads, reload B, type again, press a button; the flow must hold
/// the goto for A, the click, a wait (not a second goto) for B, B's typing, the
/// reload as a goto, and it must replay to the same end state. Page-scoped
/// recorders lose everything after the first unload.
#[tokio::test(flavor = "multi_thread")]
async fn recording_survives_navigation_and_replays() {
    let Some((b, t)) = tab().await else {
        return;
    };
    let (base, stop) = serve_site().await;
    b.navigate(&t, "goto", Some(&format!("{base}/a")))
        .await
        .expect("page a");

    RecordManager::start(b.as_ref(), &t).await.expect("start");
    act(&b, &t, "#to-b", "click", None).await;
    b.wait(&t, "navigation", None, 10_000).await.expect("to b");
    assert_eq!(
        js(&b, &t, "document.querySelector('h1').textContent").await,
        "page b"
    );
    // The recorder re-attached on the new document (badge up, listener live).
    assert_eq!(
        js(
            &b,
            &t,
            "!!document.getElementById('agentctl-recorder-badge')"
        )
        .await,
        true,
        "recorder badge missing on page b"
    );
    let st = RecordManager::status(b.as_ref(), &t).await.expect("status");
    assert_eq!(st["recording"], true, "{st}");
    assert!(
        st["event_count"].as_u64().unwrap_or(0) >= 2,
        "the click and the new document are counted: {st}"
    );
    act(&b, &t, "#b-in", "type", Some("hello")).await;
    // A reload is a new document the person made, not a click's result.
    b.navigate(&t, "reload", None).await.expect("reload");
    b.wait(&t, "navigation", None, 10_000)
        .await
        .expect("reloaded");
    act(&b, &t, "#b-in", "type", Some("world")).await;
    act(&b, &t, "#b-go", "click", None).await;
    assert_eq!(
        js(&b, &t, "document.getElementById('out').textContent").await,
        "B:world"
    );

    let steps = RecordManager::stop(b.as_ref(), &t).await.expect("stop");
    let dump = serde_json::to_string(&steps).unwrap();
    let find = |pred: &dyn Fn(&Value) -> bool| steps.iter().position(pred);
    let goto_a = find(&|s| s["op"] == "navigate" && s["url"] == format!("{base}/a").as_str())
        .unwrap_or_else(|| panic!("no goto for page a: {dump}"));
    let click = find(&|s| s["action"] == "click" && s["query"] == "to B")
        .unwrap_or_else(|| panic!("no click on the link: {dump}"));
    let wait_nav = find(&|s| s["op"] == "wait" && s["navigation"] == true)
        .unwrap_or_else(|| panic!("no wait for navigation: {dump}"));
    let type_hello = find(&|s| s["action"] == "type" && s["value"] == "hello")
        .unwrap_or_else(|| panic!("typing on page b not recorded: {dump}"));
    let goto_b = find(&|s| s["op"] == "navigate" && s["url"] == format!("{base}/b").as_str())
        .unwrap_or_else(|| panic!("the reload is not a goto: {dump}"));
    let type_world = find(&|s| s["action"] == "type" && s["value"] == "world")
        .unwrap_or_else(|| panic!("typing after the reload not recorded: {dump}"));
    let go = find(&|s| s["action"] == "click" && s["query"] == "Go")
        .unwrap_or_else(|| panic!("no click on Go: {dump}"));
    assert!(
        goto_a < click && click < wait_nav && wait_nav < type_hello,
        "{dump}"
    );
    assert_eq!(wait_nav, click + 1, "the wait follows the click: {dump}");
    assert!(
        type_hello < goto_b && goto_b < type_world && type_world < go,
        "{dump}"
    );
    // The link's navigation is waited for, never replayed as a second goto.
    let gotos_to_b = steps
        .iter()
        .filter(|s| s["op"] == "navigate" && s["url"] == format!("{base}/b").as_str())
        .count();
    assert_eq!(gotos_to_b, 1, "only the reload is a goto: {dump}");

    // Stopping unregisters the recorder: a new document gets none of it, and
    // the current one is silenced.
    assert_eq!(
        js(
            &b,
            &t,
            "!!document.getElementById('agentctl-recorder-badge')"
        )
        .await,
        false,
        "badge left on the current page"
    );
    b.navigate(&t, "goto", Some(&format!("{base}/b")))
        .await
        .expect("fresh load");
    b.wait(&t, "navigation", None, 10_000)
        .await
        .expect("loaded");
    assert_eq!(
        js(&b, &t, "typeof window.__agentctl_recorder").await,
        "undefined",
        "the new-document script outlived the recording"
    );

    // Replay from somewhere else: same end state.
    let dir = std::env::temp_dir().join(format!("agentctl-navrec-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let store = FlowStore::new(dir.join("flows.json"), 10, 100);
    let m = BrowserModule::new(b.clone()).with_flow_store(store);
    let e = m
        .call(
            "browser_flow",
            json!({ "action": "save", "name": "nav", "steps": steps }),
            &ctx(),
        )
        .await;
    assert!(e.ok, "{e:?}");
    b.navigate(&t, "goto", Some("about:blank"))
        .await
        .expect("blank");
    let e = m
        .call(
            "browser_flow",
            json!({ "action": "run", "name": "nav", "target_id": t }),
            &ctx(),
        )
        .await;
    assert!(e.ok, "replay failed: {e:?}");
    assert_eq!(
        js(&b, &t, "document.getElementById('out').textContent").await,
        "B:world",
        "replay did not reach the recorded end state"
    );
    assert_eq!(
        js(&b, &t, "location.pathname").await,
        "/b",
        "replay ended on the wrong page"
    );

    let _ = stop.send(());
    let _ = std::fs::remove_dir_all(&dir);
    let _ = b.disconnect(1, true).await;
}

/// Recording keeps a Page-domain session open, so Chrome hands that session
/// the page's dialogs. It must answer them (an unanswered `alert` freezes the
/// page for the person recording) and say what it answered.
#[tokio::test(flavor = "multi_thread")]
async fn a_dialog_during_recording_does_not_freeze_the_page_and_is_reported() {
    let Some((b, t)) = tab().await else {
        return;
    };
    let html = r#"<!doctype html><body>
<button id="ask" onclick="alert('are you sure'); document.getElementById('o').textContent='after'">Ask</button>
<div id="o"></div></body>"#;
    let (base, stop) = serve_html(html.to_string()).await;
    b.navigate(&t, "goto", Some(&base)).await.expect("navigate");
    RecordManager::start(b.as_ref(), &t).await.expect("start");
    act(&b, &t, "#ask", "click", None).await;
    assert_eq!(
        js(&b, &t, "document.getElementById('o').textContent").await,
        "after",
        "the page stayed blocked on the alert"
    );
    let res = b
        .observe_stop(&t, "true")
        .await
        .expect("stop the observation");
    let dialogs = res["dialogs"].as_array().expect("dialogs");
    assert!(
        dialogs
            .iter()
            .any(|d| d["type"] == "alert" && d["message"] == "are you sure"),
        "the recorder did not report the alert it answered: {res}"
    );
    assert_eq!(res["script_removed"], true);
    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

/// A login recorded with a password, saved through `browser_record`, replays
/// with the password supplied at run time and never lands in the flow store.
#[tokio::test(flavor = "multi_thread")]
async fn a_recorded_login_replays_with_supplied_secrets_and_stores_none() {
    let Some((b, t)) = tab().await else {
        return;
    };
    const PASSWORD: &str = "correct-horse-battery";
    let html = r#"<!doctype html><body>
<input id="user" placeholder="user">
<input id="pw" type="password">
<button id="login" onclick="document.getElementById('out').textContent=(document.getElementById('pw').value==='correct-horse-battery'?'welcome ':'denied ')+document.getElementById('user').value">Log in</button>
<div id="out"></div>
</body>"#;
    let (base, stop) = serve_html(html.to_string()).await;
    b.navigate(&t, "goto", Some(&base)).await.expect("navigate");

    let dir = std::env::temp_dir().join(format!("agentctl-login-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join("browser_flows.json");
    let m = BrowserModule::new(b.clone()).with_flow_store(FlowStore::new(file.clone(), 10, 100));
    let call = |tool: &'static str, args: Value| {
        let m = &m;
        async move { m.call(tool, args, &ctx()).await }
    };
    let store_text = || std::fs::read_to_string(&file).unwrap_or_default();

    // Record: type a username and the password, click Log in.
    let e = call(
        "browser_record",
        json!({ "target_id": t, "action": "start" }),
    )
    .await;
    assert!(e.ok, "{e:?}");
    act(&b, &t, "#user", "type", Some("ada")).await;
    act(&b, &t, "#pw", "type", Some(PASSWORD)).await;
    act(&b, &t, "#login", "click", None).await;
    assert_eq!(
        js(&b, &t, "document.getElementById('out').textContent").await,
        "welcome ada",
        "the recorded actions really logged in"
    );
    let e = call(
        "browser_record",
        json!({ "target_id": t, "action": "stop", "name": "login" }),
    )
    .await;
    assert!(e.ok, "{e:?}");
    assert!(!store_text().contains(PASSWORD), "password in flow store");
    assert!(
        !serde_json::to_string(&e).unwrap().contains(PASSWORD),
        "password in the stop result"
    );
    let flow = call("browser_flow", json!({ "action": "get", "name": "login" })).await;
    let steps = flow.data.expect("flow data")["steps"].clone();
    let pw_step = steps
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["query"] == "#pw")
        .unwrap_or_else(|| panic!("no #pw step: {steps}"));
    assert_eq!(pw_step["secret"], true);
    assert_eq!(pw_step["secret_ref"], "pw");
    assert!(pw_step.get("value").is_none(), "{pw_step}");

    let run = |secrets: Option<Value>| {
        let (m, t) = (&m, &t);
        async move {
            let mut args = json!({ "action": "run", "name": "login", "target_id": t });
            if let Some(s) = secrets {
                args["secrets"] = s;
            }
            m.call("browser_flow", args, &ctx()).await
        }
    };

    // Without the secret: refused up front, naming the ref; nothing typed.
    b.navigate(&t, "goto", Some(&base)).await.expect("reload");
    let e = run(None).await;
    assert!(!e.ok, "{e:?}");
    let msg = e.error.as_ref().unwrap().message.clone();
    assert!(msg.contains("'pw'"), "error must name the ref: {msg}");
    assert_eq!(
        js(&b, &t, "document.getElementById('user').value").await,
        ""
    );

    // With a wrong password the page denies: the supplied value is what is typed.
    let e = run(Some(json!({ "pw": "not-the-password" }))).await;
    assert!(e.ok, "the steps themselves pass: {e:?}");
    assert_eq!(
        js(&b, &t, "document.getElementById('out').textContent").await,
        "denied ada"
    );

    // With the right one the end state is the logged-in one.
    b.navigate(&t, "goto", Some(&base)).await.expect("reload");
    let e = run(Some(json!({ "pw": PASSWORD, "unused": "x" }))).await;
    assert!(e.ok, "{e:?}");
    assert_eq!(
        js(&b, &t, "document.getElementById('out').textContent").await,
        "welcome ada"
    );
    assert!(
        !serde_json::to_string(&e).unwrap().contains(PASSWORD),
        "password in the run result"
    );

    // A malformed secrets argument is refused without echoing it.
    let e = run(Some(json!({ "pw": 1234 }))).await;
    assert!(!e.ok);
    // A step that is secret AND carries a literal value cannot be saved.
    let before = store_text();
    let e = call(
        "browser_flow",
        json!({ "action": "save", "name": "bad", "steps": [
            { "op": "act", "action": "type", "query": "#pw", "secret": true, "value": "hunter2-literal" }
        ]}),
    )
    .await;
    assert!(!e.ok, "{e:?}");
    let msg = e.error.as_ref().unwrap().message.clone();
    assert!(
        msg.contains("secret_ref") && msg.contains("secrets"),
        "{msg}"
    );
    assert!(!msg.contains("hunter2-literal"), "error echoes the value");
    assert_eq!(store_text(), before, "a refused save must change nothing");

    // The password never touched the store, through any of the above.
    assert!(!store_text().contains(PASSWORD));
    let _ = stop.send(());
    let _ = std::fs::remove_dir_all(&dir);
    let _ = b.disconnect(1, true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn recorder_redaction_matches_rust_rule_and_never_captures_value() {
    let Some((b, t)) = tab().await else {
        return;
    };
    // (type, name, id suffix, autocomplete). The id is `f{i}{suffix}`, so the
    // recorded selector `#f{i}...` maps an event back to its case.
    let cases: &[(&str, &str, &str, &str)] = &[
        ("text", "", "", ""),
        ("password", "", "", ""),
        ("text", "", "", "one-time-code"),
        ("text", "", "", "current-password"),
        ("text", "", "", "new-password"),
        ("text", "", "", "cc-number"),
        ("text", "", "", "cc-csc"),
        ("email", "", "", "email"),
        ("text", "", "", "username"),
        ("text", "password", "", ""),
        ("text", "user_passwd", "", ""),
        ("text", "newPassword", "", ""),
        ("text", "pin", "", ""),
        ("text", "userPIN", "", ""),
        ("text", "otp", "", ""),
        ("text", "", "_otp-code", ""),
        ("text", "card_cvv", "", ""),
        ("text", "cvc", "", ""),
        ("text", "ssn", "", ""),
        ("text", "client_secret", "", ""),
        ("text", "", "_csrf-token", ""),
        ("text", "api_key", "", ""),
        ("text", "apiKey", "", ""),
        ("text", "", "_api-key", ""),
        ("text", "spinner", "", ""),
        ("text", "shipping", "", ""),
        ("text", "topic", "", ""),
        ("text", "monkey", "", ""),
    ];
    let mut html = String::from("<!doctype html><body>");
    for (i, (ty, name, suffix, ac)) in cases.iter().enumerate() {
        html.push_str(&format!(
            "<input id=\"f{i}{suffix}\" name=\"{name}\" type=\"{ty}\" autocomplete=\"{ac}\">"
        ));
    }
    let (base, stop) = serve_html(html).await;
    b.navigate(&t, "goto", Some(&base)).await.expect("navigate");
    RecordManager::start(b.as_ref(), &t).await.expect("start");
    for (i, case) in cases.iter().enumerate() {
        act(
            &b,
            &t,
            &format!("#f{i}{}", case.2),
            "type",
            Some(&format!("VALUE{i}")),
        )
        .await;
    }
    let raw = recorded_events(&b, &t).await;
    let events = raw["events"].as_array().expect("events");
    // `type` is a real insertion, so the browser itself adds a `change` when
    // the next field takes focus; the recorder coalesces the repeats.
    for kind in ["input", "change"] {
        let n = events.iter().filter(|e| e["kind"] == kind).count();
        assert!(n >= cases.len(), "an {kind} for each field, got {n}");
    }
    assert!(
        events.len() <= cases.len() * 3,
        "at most input + change + a blur change each, got {}",
        events.len()
    );
    // The decision must actually split both ways, or the table proves nothing.
    assert!(events.iter().any(|e| e["secret"] == true));
    assert!(events.iter().any(|e| e["secret"] == false));
    for ev in events {
        let sel = ev["selector"].as_str().unwrap();
        // A name-bearing field is located by its id first (`#f12`), which
        // carries no secret word itself; the suffix cases put it in the id.
        let digits: String = sel
            .trim_start_matches("#f")
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        let i: usize = digits.parse().unwrap();
        let (ty, name, suffix, ac) = cases[i];
        let id = format!("f{i}{suffix}");
        let expect_secret = is_secret_field(ty, name, &id, ac);
        assert_eq!(ev["secret"], expect_secret, "{sel} {:?}", cases[i]);
        if expect_secret {
            assert!(
                ev["value"].is_null(),
                "secret value captured for {sel}: {ev}"
            );
        } else {
            assert_eq!(ev["value"], format!("VALUE{i}"));
        }
    }
    assert!(
        !raw.to_string().contains("VALUE1\""),
        "password value in envelope"
    );
    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

/// What a click may leave behind. Text is kept only for a button or link
/// label that is not in a secret or `data-private` context; clicking an input
/// or textarea (whose `innerText` is its value) must record no text at all.
#[tokio::test(flavor = "multi_thread")]
async fn clicked_text_is_kept_only_for_plain_labels() {
    let Some((b, t)) = tab().await else {
        return;
    };
    let html = r##"<!doctype html><body>
<button id="save">Save</button>
<a id="docs" href="#docs">Docs</a>
<textarea id="note">my private diary entry</textarea>
<input id="plain" value="typed value">
<input id="pw" type="password" value="hunter2-pw">
<div data-private><button id="reveal">Show card</button></div>
<div autocomplete="cc-number"><button id="pay">Pay now</button></div>
<div id="card_pin_box"><button id="pinbtn">Reveal pin</button></div>
<button id="long">This label is far too long to be a locator</button>
</body>"##;
    let (base, stop) = serve_html(html.to_string()).await;
    b.navigate(&t, "goto", Some(&base)).await.expect("navigate");
    RecordManager::start(b.as_ref(), &t).await.expect("start");
    for id in [
        "save", "docs", "note", "plain", "pw", "reveal", "pay", "pinbtn", "long",
    ] {
        act(&b, &t, &format!("#{id}"), "click", None).await;
    }
    let raw = recorded_events(&b, &t).await;
    let events = raw["events"].as_array().expect("events");
    let text_of = |sel: &str| -> Value {
        events
            .iter()
            .find(|e| e["kind"] == "click" && e["selector"] == sel)
            .unwrap_or_else(|| panic!("no click event for {sel}: {raw}"))["text"]
            .clone()
    };
    assert_eq!(text_of("#save"), "Save");
    assert_eq!(text_of("#docs"), "Docs");
    for sel in [
        "#note", "#plain", "#pw", "#reveal", "#pay", "#pinbtn", "#long",
    ] {
        assert!(text_of(sel).is_null(), "{sel} kept text: {}", text_of(sel));
    }
    let dump = raw.to_string();
    for leaked in [
        "private diary",
        "typed value",
        "hunter2",
        "Show card",
        "Pay now",
    ] {
        assert!(!dump.contains(leaked), "{leaked} leaked: {dump}");
    }
    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn challenge_detect_and_clearance_use_the_real_eval_shape() {
    let Some((b, t)) = tab().await else {
        return;
    };
    // An OTP input that a "human" fills in once the wait has begun. The fill
    // is triggered by the test, not a page timer, so a slow load cannot let
    // it land before detection.
    let html = r#"<!doctype html><body>
<input id="otp" name="otp" autocomplete="one-time-code">
<script>window.__humanFills = function(){ setTimeout(function(){ document.getElementById('otp').value='123456'; }, 300); };</script>
</body>"#;
    let (base, stop) = serve_html(html.to_string()).await;
    b.navigate(&t, "goto", Some(&base)).await.expect("navigate");

    let st = ChallengeManager::detect(b.as_ref(), &t)
        .await
        .expect("detect");
    assert!(st.detected, "OTP input must be detected: {st:?}");
    assert_eq!(st.kind, Some(ChallengeKind::Otp2fa));

    js(&b, &t, "window.__humanFills(), true").await;
    let res = ChallengeManager::wait_for_clearance(b.as_ref(), &t, 10_000)
        .await
        .expect("clearance");
    assert_eq!(res["detected"], true);
    assert_eq!(res["cleared"], true);
    assert_eq!(res["kind"], "otp_2fa");
    assert_eq!(
        js(
            &b,
            &t,
            "!!document.getElementById('agentctl-challenge-hud')"
        )
        .await,
        false,
        "HUD removed after clearance"
    );
    let st = ChallengeManager::detect(b.as_ref(), &t)
        .await
        .expect("detect");
    assert!(!st.detected);

    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn browser_wait_network_idle_is_accepted_and_settles() {
    let Some((b, t)) = tab().await else {
        return;
    };
    let (base, stop) = serve_html("<!doctype html><body>idle</body>".to_string()).await;
    b.navigate(&t, "goto", Some(&base)).await.expect("navigate");
    let m = BrowserModule::new(b.clone());
    for args in [
        json!({ "target_id": t, "network_idle": true, "timeout_ms": 5000 }),
        json!({ "target_id": t, "condition": "network_idle", "timeout_ms": 5000 }),
    ] {
        let e = m.call("browser_wait", args, &ctx()).await;
        assert!(e.ok, "{e:?}");
        let d = e.data.unwrap();
        assert_eq!(d["condition"], "network_idle");
        assert_eq!(d["settled"], true);
    }
    let _ = stop.send(());
    let _ = b.disconnect(1, true).await;
}

/// `tabs open` loads its URL as Chrome creates the tab, so it must obey the
/// same navigation policy as `navigate goto`, and a refused open must not
/// leave a tab behind.
#[tokio::test]
async fn tabs_open_obeys_the_navigation_policy() {
    if !have_chrome() {
        return;
    }
    let b = CdpBackend::new(NavPolicy::new(
        &["https://allowed.example".to_string()],
        false,
    ));
    b.connect(None, Some(json!({ "headless": true, "port": 0 })))
        .await
        .expect("launch");
    let count = |v: Value| v["tabs"].as_array().map(|a| a.len()).unwrap_or(0);
    let before = count(b.tabs(1, "list", None, None).await.expect("list"));

    let refused = b
        .tabs(1, "open", None, Some("https://elsewhere.example/"))
        .await;
    assert!(
        matches!(refused, Err(mcp_browser::BrowserError::PermissionDenied(_))),
        "{refused:?}"
    );
    let after = count(b.tabs(1, "list", None, None).await.expect("list"));
    assert_eq!(after, before, "a refused open must not create a tab");

    // A blank tab is still allowed.
    b.tabs(1, "open", None, None).await.expect("blank tab");
    let _ = b.disconnect(1, true).await;
}
