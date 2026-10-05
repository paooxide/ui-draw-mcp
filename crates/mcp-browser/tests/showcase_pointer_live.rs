//! Live tests (real headless Chrome) for the showcase pointer: that the page
//! sees real mouse movement, that `hover` applies `:hover`, that the overlay
//! renders on a Trusted Types page, and that `browser_showcase` says so when
//! it could not draw anything.
//!
//! Skipped when `AGENTCTL_SKIP_LIVE` is set or no Chrome binary is found.

use mcp_browser::showcase::{CursorStyle, ShowcaseConfig};
use mcp_browser::{BrowserBackend, CdpBackend, Locator, NavPolicy, CHROME_BINS};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn have_chrome() -> bool {
    let skip = std::env::var_os("AGENTCTL_SKIP_LIVE").is_some_and(|v| v != "0");
    !skip && CHROME_BINS.iter().any(|p| std::path::Path::new(p).exists())
}

/// `/` is the fixture page; `/tt` is the same page under a Trusted Types CSP.
const PAGE: &str = "<!doctype html><body style=\"margin:0\">\
<style>#h{position:absolute;left:300px;top:200px;width:120px;height:60px;background:rgb(255,0,0)}\
#h:hover{background:rgb(0,128,0)}</style>\
<button id=\"b\" style=\"position:absolute;left:500px;top:300px;width:100px;height:40px\">go</button>\
<div id=\"h\">hover me</div>\
<script>window.__moves=[];window.__clicks=0;\
window.addEventListener('mousemove',function(e){window.__moves.push(e.isTrusted)},true);\
document.getElementById('b').addEventListener('click',function(){window.__clicks++});</script></body>";

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
                        let n = stream.read(&mut buf).await.unwrap_or(0);
                        let req = String::from_utf8_lossy(&buf[..n]).to_string();
                        let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                        let csp = if path.starts_with("/tt") {
                            "Content-Security-Policy: require-trusted-types-for 'script'\r\n"
                        } else {
                            ""
                        };
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n{csp}Content-Length: {}\r\nConnection: close\r\n\r\n{PAGE}",
                            PAGE.len()
                        );
                        let _ = stream.write_all(resp.as_bytes()).await;
                    });
                }
            }
        }
    });
    (format!("http://127.0.0.1:{port}"), tx)
}

async fn tab() -> Option<(CdpBackend, u32, String)> {
    if !have_chrome() {
        return None;
    }
    let b = CdpBackend::new(NavPolicy::new(&[], true));
    let c = b
        .connect(None, Some(json!({ "headless": true, "port": 0 })))
        .await
        .ok()?;
    let id = c["browser_id"].as_u64()? as u32;
    let tabs = b.tabs(id, "list", None, None).await.ok()?;
    let target = tabs["tabs"][0]["target_id"].as_str()?.to_string();
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

async fn js(b: &CdpBackend, t: &str, expr: &str) -> Value {
    b.eval(t, expr).await.unwrap()["result"].clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_act_moves_the_real_pointer_and_draws_a_cursor_of_the_configured_size() {
    let Some((b, id, t)) = tab().await else {
        return;
    };
    let (base, stop) = serve().await;
    b.navigate(&t, "goto", Some(&format!("{base}/")))
        .await
        .unwrap();

    let mut cfg = ShowcaseConfig::snappy();
    cfg.cursor_size = 48;
    cfg.cursor_style = CursorStyle::NeonCyan;
    let shown = b.showcase(&t, Some(cfg)).await.unwrap();
    assert_eq!(shown["rendered"], true, "{shown}");
    assert_eq!(shown["cursor_size"], 48);
    assert!(shown.get("warning").is_none(), "{shown}");

    let done = b.act(&t, sel("#b"), "click", None).await.unwrap();
    assert_eq!(done["showcase"], true);
    assert_eq!(done["showcase_rendered"], true, "{done}");

    // The click still happened, and the page saw the pointer travel: several
    // `mousemove`s, every one trusted (a real input event, not a dispatch).
    assert_eq!(js(&b, &t, "window.__clicks").await, 1);
    let moves = js(&b, &t, "window.__moves").await;
    let moves = moves.as_array().expect("moves");
    assert!(moves.len() > 1, "page saw {} mousemove events", moves.len());
    assert!(moves.iter().all(|m| m == true), "untrusted move: {moves:?}");

    // The cursor exists at the configured size, and finished over the button.
    assert_eq!(
        js(
            &b,
            &t,
            "document.getElementById('agentctl_showcase_cursor').getBoundingClientRect().width"
        )
        .await,
        48
    );
    let at = js(
        &b,
        &t,
        "(function(){var r=document.getElementById('agentctl_showcase_cursor').getBoundingClientRect();\
         return [Math.round(r.left),Math.round(r.top)]})()",
    )
    .await;
    // The arrow's tip is the click point (button centre 550,320); the box is
    // offset a hair so the tip lands there.
    assert!((at[0].as_f64().unwrap() - 550.0).abs() < 8.0, "{at}");
    assert!((at[1].as_f64().unwrap() - 320.0).abs() < 8.0, "{at}");

    // A second act starts from where the first ended, not from the corner.
    let before = js(&b, &t, "window.__moves.length").await.as_u64().unwrap();
    b.act(&t, sel("#h"), "hover", None).await.unwrap();
    let after = js(&b, &t, "window.__moves.length").await.as_u64().unwrap();
    assert!(after > before + 1, "no movement from {before} to {after}");

    let _ = stop.send(());
    b.disconnect(id, true).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn hover_applies_css_hover_with_and_without_showcase() {
    let Some((b, id, t)) = tab().await else {
        return;
    };
    let (base, stop) = serve().await;
    let url = format!("{base}/");
    b.navigate(&t, "goto", Some(&url)).await.unwrap();
    let bg = "getComputedStyle(document.getElementById('h')).backgroundColor";
    assert_eq!(js(&b, &t, bg).await, "rgb(255, 0, 0)");

    // Showcase off: the pointer still really moves.
    b.act(&t, sel("#h"), "hover", None).await.unwrap();
    assert_eq!(js(&b, &t, bg).await, "rgb(0, 128, 0)", ":hover not applied");

    // Showcase on, on a fresh load of the page.
    b.navigate(&t, "goto", Some(&url)).await.unwrap();
    assert_eq!(js(&b, &t, bg).await, "rgb(255, 0, 0)");
    b.showcase(&t, Some(ShowcaseConfig::snappy()))
        .await
        .unwrap();
    b.act(&t, sel("#h"), "hover", None).await.unwrap();
    assert_eq!(
        js(&b, &t, bg).await,
        "rgb(0, 128, 0)",
        ":hover not applied under showcase"
    );

    let _ = stop.send(());
    b.disconnect(id, true).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_overlay_renders_on_a_trusted_types_page() {
    let Some((b, id, t)) = tab().await else {
        return;
    };
    let (base, stop) = serve().await;
    b.navigate(&t, "goto", Some(&format!("{base}/tt")))
        .await
        .unwrap();
    // The page really enforces Trusted Types: innerHTML is refused.
    let blocked = js(
        &b,
        &t,
        "(function(){try{document.createElement('div').innerHTML='<b>x</b>';return false}catch(e){return true}})()",
    )
    .await;
    assert_eq!(blocked, true, "fixture CSP is not enforced");

    let shown = b.showcase(&t, Some(ShowcaseConfig::demo())).await.unwrap();
    assert_eq!(shown["rendered"], true, "{shown}");
    assert!(shown.get("warning").is_none(), "{shown}");
    assert_eq!(
        js(
            &b,
            &t,
            "!!document.getElementById('agentctl_showcase_cursor')"
        )
        .await,
        true
    );

    let done = b.act(&t, sel("#b"), "click", None).await.unwrap();
    assert_eq!(done["showcase_rendered"], true, "{done}");
    assert_eq!(js(&b, &t, "window.__clicks").await, 1);

    let _ = stop.send(());
    b.disconnect(id, true).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn showcase_says_so_when_nothing_could_be_drawn() {
    let Some((b, id, t)) = tab().await else {
        return;
    };
    b.navigate(&t, "goto", Some("about:blank")).await.unwrap();

    // A page without a body (as while it is still loading).
    js(&b, &t, "document.body.remove(); true").await;
    let shown = b.showcase(&t, Some(ShowcaseConfig::demo())).await.unwrap();
    assert_eq!(shown["enabled"], true);
    assert_eq!(shown["rendered"], false, "{shown}");
    let warning = shown["warning"].as_str().expect("warning");
    assert!(warning.contains("<body>"), "{warning}");

    // No tab at all.
    let none = b.showcase("", Some(ShowcaseConfig::demo())).await.unwrap();
    assert_eq!(none["rendered"], false);
    assert!(none["warning"].as_str().is_some());

    // A tab that does not exist: the connection error is in the warning.
    let gone = b
        .showcase("no-such-tab", Some(ShowcaseConfig::demo()))
        .await
        .unwrap();
    assert_eq!(gone["rendered"], false);
    assert!(gone["warning"].as_str().is_some_and(|w| w.len() > 30));

    // Disabled: nothing was asked for, so nothing is reported missing.
    let off = b
        .showcase(&t, Some(ShowcaseConfig::default()))
        .await
        .unwrap();
    assert_eq!(off["enabled"], false);
    assert!(off.get("rendered").is_none());

    b.disconnect(id, true).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_pointer_glide_is_not_recorded_as_user_steps() {
    let Some((b, id, t)) = tab().await else {
        return;
    };
    let (base, stop) = serve().await;
    b.navigate(&t, "goto", Some(&format!("{base}/")))
        .await
        .unwrap();
    b.showcase(&t, Some(ShowcaseConfig::snappy()))
        .await
        .unwrap();
    mcp_browser::RecordManager::start(&b, &t).await.unwrap();

    // While recording the act script runs in the recorder's isolated world;
    // the overlay it draws is DOM, so the page still has the cursor.
    b.act(&t, sel("#h"), "hover", None).await.unwrap();
    let done = b.act(&t, sel("#b"), "click", None).await.unwrap();
    assert_eq!(done["showcase_rendered"], true, "{done}");
    assert_eq!(
        js(
            &b,
            &t,
            "!!document.getElementById('agentctl_showcase_cursor')"
        )
        .await,
        true
    );
    assert!(js(&b, &t, "window.__moves.length").await.as_u64().unwrap() > 1);

    let events = mcp_browser::RecordManager::stop_raw(&b, &t).await.unwrap();
    let steps: Vec<String> = events
        .iter()
        .filter(|e| e.kind != "navigate")
        .map(|e| format!("{}:{}", e.kind, e.selector))
        .collect();
    assert_eq!(steps, vec!["click:#b".to_string()], "{steps:?}");

    let _ = stop.send(());
    b.disconnect(id, true).await.unwrap();
}
