//! Real-Chrome coverage for the showcase typing HUD.
//!
//! The HUD runs inside the page's own origin and is fed the value an agent is
//! typing, so two properties are only meaningful against a live browser: the
//! value must never be parsed as HTML (DOM XSS), and a secret must never be
//! put on screen. Skips (rather than fails) when no Chromium is installed or
//! `AGENTCTL_SKIP_LIVE=1`. Uses CDP ports 9480-9489 only.

use mcp_browser::showcase::ShowcaseConfig;
use mcp_browser::{BrowserBackend, CdpBackend, Locator, NavPolicy, CHROME_BINS};
use serde_json::{json, Value};

fn have_chrome() -> bool {
    let skip = std::env::var_os("AGENTCTL_SKIP_LIVE").is_some_and(|v| v != "0");
    !skip && CHROME_BINS.iter().any(|p| std::path::Path::new(p).exists())
}

const PAGE: &str = r#"document.body.innerHTML =
  '<input id="t" type="text"><input id="p" type="password">' +
  '<input id="otp" autocomplete="one-time-code">'; true"#;

/// Launch a throwaway headless browser on `port`, with the fixture page and
/// showcase on. Returns `(backend, browser_id, target_id)`.
async fn page(port: u64) -> Option<(CdpBackend, u32, String)> {
    if !have_chrome() {
        return None;
    }
    let b = CdpBackend::new(NavPolicy::default());
    let c = b
        .connect(None, Some(json!({ "headless": true, "port": port })))
        .await
        .ok()?;
    let id = c["browser_id"].as_u64()? as u32;
    let tabs = b.tabs(id, "list", None, None).await.ok()?;
    let target = tabs["tabs"][0]["target_id"].as_str()?.to_string();
    b.navigate(&target, "goto", Some("about:blank"))
        .await
        .ok()?;
    b.eval(&target, PAGE).await.ok()?;
    b.showcase(&target, Some(ShowcaseConfig::snappy()))
        .await
        .ok()?;
    Some((b, id, target))
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

async fn text_of(b: &CdpBackend, t: &str, expr: &str) -> String {
    b.eval(t, expr).await.unwrap()["result"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

const HUD: &str = "(document.getElementById('agentctl_showcase_hud')||{}).textContent||''";

#[tokio::test(flavor = "multi_thread")]
async fn agent_text_is_never_parsed_as_html() {
    let Some((b, id, t)) = page(9480).await else {
        return;
    };
    let payload = r#"<img src=x onerror="window.__pwned=1">"#;
    b.act(&t, sel("#t"), "type", Some(payload)).await.unwrap();
    // Let a would-be onerror handler run before looking.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let pwned = b.eval(&t, "typeof window.__pwned").await.unwrap();
    assert_eq!(pwned["result"], "undefined", "HUD text executed as HTML");
    let hud_imgs = b
        .eval(
            &t,
            "document.getElementById('agentctl_showcase_hud').querySelectorAll('img').length",
        )
        .await
        .unwrap();
    assert_eq!(hud_imgs["result"], 0);
    // It is shown, as text, which is the point of the HUD.
    assert!(text_of(&b, &t, HUD).await.contains("<img"));
    b.disconnect(id, true).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn secrets_are_masked_and_the_hud_does_not_linger() {
    let Some((b, id, t)) = page(9481).await else {
        return;
    };
    let secret = "hunter2-SECRET";

    // A password input is masked without being told.
    b.act(&t, sel("#p"), "type", Some(secret)).await.unwrap();
    let hud = text_of(&b, &t, HUD).await;
    assert!(!hud.contains(secret), "password shown in HUD: {hud}");
    assert!(hud.contains('\u{2022}'), "expected a mask, got: {hud}");

    // So is a one-time-code field, and any field when the call says so.
    b.act(&t, sel("#otp"), "type", Some("123456"))
        .await
        .unwrap();
    assert!(!text_of(&b, &t, HUD).await.contains("123456"));
    b.act_masked(&t, sel("#t"), "type", Some("api-key-xyz"), true)
        .await
        .unwrap();
    assert!(!text_of(&b, &t, HUD).await.contains("api-key-xyz"));

    // A plain field is still shown, so masking is not simply "hide everything".
    b.act(&t, sel("#t"), "type", Some("visible-text"))
        .await
        .unwrap();
    assert!(text_of(&b, &t, HUD).await.contains("visible-text"));

    // The real value still reached the field.
    let v = b.eval(&t, "document.getElementById('p').value").await;
    assert_eq!(v.unwrap()["result"], secret);

    // After the fade the text is removed, not merely hidden.
    tokio::time::sleep(std::time::Duration::from_millis(2300)).await;
    assert_eq!(text_of(&b, &t, HUD).await, "");
    let body = text_of(&b, &t, "document.body.innerText").await;
    assert!(!body.contains("visible-text"), "HUD lingers: {body}");
    b.disconnect(id, true).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn fill_form_shows_the_value_and_masks_passwords() {
    let Some((b, id, t)) = page(9482).await else {
        return;
    };
    let fields: Value = json!([{ "selector": "#t", "value": "alice" }]);
    b.fill_form(&t, &fields, None).await.unwrap();
    // Previously the snippet ran before `val` was assigned: `Type ""`.
    assert!(text_of(&b, &t, HUD).await.contains("alice"));

    let fields: Value = json!([{ "selector": "#p", "value": "hunter2-SECRET" }]);
    b.fill_form(&t, &fields, None).await.unwrap();
    let hud = text_of(&b, &t, HUD).await;
    assert!(!hud.contains("hunter2-SECRET"), "{hud}");

    let fields: Value = json!([{ "selector": "#t", "value": "tok-123", "secret": true }]);
    b.fill_form(&t, &fields, None).await.unwrap();
    assert!(!text_of(&b, &t, HUD).await.contains("tok-123"));
    b.disconnect(id, true).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn disabling_showcase_removes_the_overlay() {
    let Some((b, id, t)) = page(9483).await else {
        return;
    };
    b.act(&t, sel("#t"), "type", Some("hello")).await.unwrap();
    let present = "String(!!document.getElementById('__agentctl_showcase_root'))";
    assert_eq!(text_of(&b, &t, present).await, "true");

    let off = ShowcaseConfig::default();
    b.showcase(&t, Some(off)).await.unwrap();
    assert_eq!(text_of(&b, &t, present).await, "false");
    assert_eq!(
        text_of(&b, &t, "typeof window.__agentctl_showcase").await,
        "undefined"
    );

    // Re-enabling works from a clean slate.
    b.showcase(&t, Some(ShowcaseConfig::snappy()))
        .await
        .unwrap();
    assert_eq!(text_of(&b, &t, present).await, "true");

    // An attached browser outlives us; disconnect (without kill) must not
    // leave the overlay in its pages. Launched browsers are killed below.
    b.disconnect(id, false).await.unwrap();
    // The browser is still ours to reap at shutdown, but the tab is
    // unreachable by id now, so check via a fresh connection to its port.
    let b2 = CdpBackend::new(NavPolicy::default());
    let c = b2.connect(Some(9483), None).await.unwrap();
    let id2 = c["browser_id"].as_u64().unwrap() as u32;
    let tabs = b2.tabs(id2, "list", None, None).await.unwrap();
    let t2 = tabs["tabs"][0]["target_id"].as_str().unwrap().to_string();
    assert_eq!(text_of(&b2, &t2, present).await, "false");
    b.shutdown();
}
