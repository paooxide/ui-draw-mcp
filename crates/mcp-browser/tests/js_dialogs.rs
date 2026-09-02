//! Real-Chrome coverage for JavaScript dialog handling.
//!
//! This cannot be tested against a stub. The failure it guards is a property of
//! Chrome itself: once a client enables the `Page` domain, Chrome stops showing
//! the native dialog and delegates it, so a client that ignores
//! `javascriptDialogOpening` leaves the renderer blocked and every later call
//! against that tab times out. Only a live browser exercises that.
//!
//! Skips (rather than fails) when no Chromium binary is installed.

use mcp_browser::{BrowserBackend, CdpBackend, DialogPolicy};
use serde_json::{json, Value};

const CHROME_BINS: &[&str] = &[
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
    "/usr/bin/google-chrome",
    "/usr/bin/chromium",
    "/usr/bin/chromium-browser",
];

/// Launching a real browser is the point of this file locally, and an
/// availability gamble on a hosted CI runner. `AGENTCTL_SKIP_LIVE=1` skips
/// every live test in the workspace.
fn skip_live() -> bool {
    std::env::var_os("AGENTCTL_SKIP_LIVE").is_some_and(|v| v != "0")
}

fn have_chrome() -> bool {
    !skip_live() && CHROME_BINS.iter().any(|p| std::path::Path::new(p).exists())
}

/// Launch a throwaway headless browser and return `(backend, target_id)`.
async fn tab(port: u64) -> Option<(CdpBackend, String)> {
    if !have_chrome() {
        return None;
    }
    let b = CdpBackend::new(Vec::new());
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
    b.navigate(&target, "goto", Some("about:blank"))
        .await
        .ok()?;
    Some((b, target))
}

fn dialog_of(v: &Value) -> Option<&Value> {
    v.get("dialogs")?.as_array()?.first()
}

/// The regression that matters: script after an `alert()` must still run, and
/// the call must return promptly rather than sit until the CDP timeout.
#[tokio::test(flavor = "multi_thread")]
async fn alert_does_not_wedge_the_tab() {
    let Some((b, t)) = tab(9351).await else {
        return;
    };

    let started = std::time::Instant::now();
    let r = b
        .eval(&t, "alert('blocked?'); 42")
        .await
        .expect("eval must return, not time out");
    assert_eq!(r["result"], 42, "code after alert() must still run");
    assert!(
        started.elapsed().as_secs() < 5,
        "eval took {:?} — the dialog was not answered",
        started.elapsed()
    );

    let d = dialog_of(&r).expect("the dialog must be reported, not silently eaten");
    assert_eq!(d["type"], "alert");
    assert_eq!(d["message"], "blocked?");
    assert_eq!(d["answered"], "dismissed");

    // And the tab is still usable afterwards.
    let after = b.eval(&t, "1+1").await.expect("tab must survive");
    assert_eq!(after["result"], 2);
}

/// Dismissal is the safe answer: a page's own `confirm()` must not become "yes"
/// just because an agent happened to evaluate something.
#[tokio::test(flavor = "multi_thread")]
async fn confirm_defaults_to_no_and_prompt_to_null() {
    let Some((b, t)) = tab(9352).await else {
        return;
    };

    let c = b.eval(&t, "confirm('delete everything?')").await.unwrap();
    assert_eq!(c["result"], false, "confirm must default to cancel");
    assert_eq!(dialog_of(&c).unwrap()["message"], "delete everything?");

    let p = b.eval(&t, "prompt('name?','fallback')").await.unwrap();
    assert!(p["result"].is_null(), "prompt must default to cancel");
    assert_eq!(dialog_of(&p).unwrap()["default_prompt"], "fallback");
}

/// Accepting is opt-in per target and sticks for later calls; supplied text
/// reaches `prompt()`.
#[tokio::test(flavor = "multi_thread")]
async fn accept_policy_is_per_target_and_supplies_prompt_text() {
    let Some((b, t)) = tab(9353).await else {
        return;
    };

    let set = b
        .dialog(&t, Some(DialogPolicy::Accept(Some("typed".into()))))
        .await
        .unwrap();
    assert_eq!(set["policy"], "accept");

    let c = b.eval(&t, "confirm('proceed?')").await.unwrap();
    assert_eq!(c["result"], true, "policy must apply to a later call");
    assert_eq!(dialog_of(&c).unwrap()["answered"], "accepted");

    let p = b.eval(&t, "prompt('name?')").await.unwrap();
    assert_eq!(p["result"], "typed", "prompt text must be delivered");

    // Reverting is just as easy, and the log survives the change.
    let back = b.dialog(&t, Some(DialogPolicy::Dismiss)).await.unwrap();
    assert_eq!(back["policy"], "dismiss");
    assert!(
        back["seen"].as_array().map(|a| a.len()).unwrap_or(0) >= 2,
        "answered dialogs must be recorded for the agent to review"
    );
    let c = b.eval(&t, "confirm('proceed?')").await.unwrap();
    assert_eq!(c["result"], false);
}

/// Request logging against a live browser. The tab must still work afterwards —
/// a client that reads events for three seconds must not lose its place in the
/// protocol.
#[tokio::test(flavor = "multi_thread")]
async fn network_log_records_real_requests() {
    let Some((b, t)) = tab(9354).await else {
        return;
    };

    // Kick off fetches while the log window is open.
    let target = t.clone();
    let bg = async {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let _ = b
            .eval(
                &target,
                "fetch('data:text/plain,hello').catch(()=>{}); \
                 fetch('https://127.0.0.1:1/nope').catch(()=>{}); 1",
            )
            .await;
    };
    let logging = b.network(&t, "log", None, None, Some(2000));
    let (log, _) = tokio::join!(logging, bg);
    let log = log.expect("log must return");
    assert_eq!(log["window_ms"], 2000);
    assert!(log["requests"].is_array(), "{log}");

    // Header values are exactly where a session token lives; the log must not
    // carry them.
    let text = serde_json::to_string(&log).unwrap();
    assert!(
        !text.contains("\"headers\""),
        "headers leaked into the log: {text}"
    );

    let after = b.eval(&t, "1+1").await.expect("tab still usable");
    assert_eq!(after["result"], 2);
}

/// `intercept` blocks by URL pattern, and clears when given an empty list.
#[tokio::test(flavor = "multi_thread")]
async fn intercept_blocks_and_clears_url_patterns() {
    let Some((b, t)) = tab(9355).await else {
        return;
    };

    let set = b
        .network(
            &t,
            "intercept",
            None,
            Some(json!({ "block": ["*.example-blocked.test/*"] })),
            None,
        )
        .await
        .expect("intercept must apply");
    assert_eq!(set["count"], 1);
    assert_eq!(set["blocked_patterns"][0], "*.example-blocked.test/*");

    let cleared = b
        .network(&t, "intercept", None, Some(json!({ "block": [] })), None)
        .await
        .expect("clearing must work");
    assert_eq!(cleared["count"], 0);
    assert_eq!(cleared["note"], "blocking cleared");
}
