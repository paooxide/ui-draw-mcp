//! Real-Chrome coverage for the state-moving browser tools: branch
//! commit/discard, checkpoint rollback, `within` scoping, `htmx_settled` and
//! profile auto-connect. The failure these guard is a tool reporting success
//! for work that did not happen, which only a live browser can show.
//!
//! Every launch lets Chrome pick its own CDP port, so the tests can run in
//! parallel. Skips (rather than fails) when `AGENTCTL_SKIP_LIVE=1` or no
//! Chromium binary is installed.

mod common;

use std::sync::Arc;
use std::time::Duration;

use mcp_browser::{
    BrowserBackend, BrowserError, BrowserModule, CdpBackend, Locator, NavPolicy, ProfileStore,
    CHROME_BINS,
};
use mcp_types::{CallCtx, CancelToken, ErrorCode, ToolModule};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn skip_live() -> bool {
    std::env::var_os("AGENTCTL_SKIP_LIVE").is_some_and(|v| v != "0")
}

fn have_chrome() -> bool {
    !skip_live() && CHROME_BINS.iter().any(|p| std::path::Path::new(p).exists())
}

/// Launch a headless Chrome on a port of its own choosing and return the
/// backend, its first tab and the port Chrome picked.
async fn launch() -> Option<(CdpBackend, String, u64)> {
    if !have_chrome() {
        return None;
    }
    let b = CdpBackend::new(NavPolicy::new(&[], true));
    let conn = b
        .connect(None, Some(json!({ "headless": true, "port": 0 })))
        .await
        .ok()?;
    let port = conn["port"].as_u64()?;
    let target = first_tab(&b, 1).await?;
    Some((b, target, port))
}

async fn first_tab(b: &CdpBackend, browser_id: u32) -> Option<String> {
    let tabs = b.tabs(browser_id, "list", None, None).await.ok()?;
    Some(
        tabs.get("tabs")?
            .as_array()?
            .first()?
            .get("target_id")?
            .as_str()?
            .to_string(),
    )
}

/// Serve fixed HTML per path on a loopback port; dropping the sender (or
/// sending on it) closes the listener so later requests are refused.
async fn serve(routes: Vec<(&'static str, String)>) -> (String, tokio::sync::oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
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
                    // One task per connection: Chrome opens speculative
                    // connections that never send a request, and a serial
                    // read would block the loop (and the shutdown) on them.
                    let routes = routes.clone();
                    let mut down = down_rx.clone();
                    tokio::spawn(async move {
                        let mut buf = [0u8; 2048];
                        // Idle speculative sockets must not outlive the server, or
                        // Chrome reuses one and the server is never really "down":
                        // they are closed the moment the server stops.
                        let n = tokio::select! {
                            _ = down.changed() => 0,
                            r = stream.read(&mut buf) => r.unwrap_or(0),
                        };
                        if n == 0 {
                            return;
                        }
                        let req = String::from_utf8_lossy(&buf[..n]).to_string();
                        let path = req
                            .lines()
                            .next()
                            .and_then(|l| l.split_whitespace().nth(1))
                            .unwrap_or("/")
                            .split('?')
                            .next()
                            .unwrap_or("/")
                            .to_string();
                        let (status, body) = match routes.iter().find(|(p, _)| *p == path) {
                            Some((_, html)) => ("200 OK", html.clone()),
                            None => ("404 Not Found", "not found".to_string()),
                        };
                        let resp = format!(
                            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = stream.write_all(resp.as_bytes()).await;
                        let _ = stream.flush().await;
                    });
                }
            }
        }
    });
    (format!("http://127.0.0.1:{}", addr.port()), tx)
}

async fn eval_js(b: &CdpBackend, target: &str, expr: &str) -> Value {
    let res = b.eval(target, expr).await.expect("eval failed");
    res.get("result").cloned().unwrap_or(Value::Null)
}

/// Navigate and wait until the page at `url` has finished loading.
async fn goto(b: &CdpBackend, target: &str, url: &str) {
    b.navigate(target, "goto", Some(url))
        .await
        .expect("navigate failed");
    common::wait_until(
        &format!("{url} to finish loading"),
        Duration::from_secs(5),
        || async {
            let v = eval_js(b, target, "location.href + '|' + document.readyState").await;
            v.as_str() == Some(&format!("{url}|complete"))
        },
    )
    .await;
}

/// GET a DevTools HTTP endpoint. Chrome keeps the connection alive and ignores
/// `Connection: close`, so read the headers and then exactly `Content-Length`.
async fn devtools_get(port: u64, path: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port as u16))
        .await
        .expect("connect devtools http");
    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = s.read(&mut tmp).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        let text = String::from_utf8_lossy(&buf).to_string();
        if let Some((head, body)) = text.split_once("\r\n\r\n") {
            let len = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            if body.len() >= len {
                return body.to_string();
            }
        }
    }
    String::new()
}

/// Ids of every target the browser on `port` currently lists (the HTTP twin of
/// `Target.getTargets`).
async fn target_ids(port: u64) -> Vec<String> {
    let body = devtools_get(port, "/json/list").await;
    let v: Value = serde_json::from_str(body.trim()).expect("/json/list returned json");
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|t| t.get("id").and_then(Value::as_str).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

async fn close_out_of_band(port: u64, target: &str) {
    devtools_get(port, &format!("/json/close/{target}")).await;
}

fn page(body: &str) -> String {
    format!("<!DOCTYPE html><html><head><title>t</title></head><body>{body}</body></html>")
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

// --- branches ---------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn branch_commit_moves_url_and_storage_to_parent_and_closes_branch() {
    let Some((b, parent, port)) = launch().await else {
        return;
    };
    let (base, _stop) = serve(vec![("/p1", page("one")), ("/p2", page("two"))]).await;
    let (p1, p2) = (format!("{base}/p1"), format!("{base}/p2"));
    goto(&b, &parent, &p1).await;
    eval_js(&b, &parent, "localStorage.setItem('cart','parent'); true").await;

    let created = b.branch_create(&parent, "try_two").await.expect("create");
    assert_eq!(created["isolated_context"], json!(true));
    assert_eq!(created["state_restored"], json!(true), "{created}");
    let branch_tab = created["branch_target_id"].as_str().unwrap().to_string();
    // The branch really got a copy of the parent's storage, in its own context.
    assert_eq!(
        eval_js(&b, &branch_tab, "localStorage.getItem('cart')").await,
        json!("parent")
    );

    goto(&b, &branch_tab, &p2).await;
    eval_js(
        &b,
        &branch_tab,
        "localStorage.setItem('cart','from_branch'); document.cookie='bc=1; path=/'; true",
    )
    .await;
    // Isolation: the parent has not seen any of it yet.
    assert_eq!(
        eval_js(&b, &parent, "localStorage.getItem('cart')").await,
        json!("parent")
    );

    let committed = b.branch_commit("try_two").await.expect("commit");
    assert_eq!(committed["committed"], json!(true));
    assert_eq!(committed["branch_closed"], json!(true));
    assert_eq!(
        eval_js(&b, &parent, "location.href").await,
        json!(p2),
        "parent must be on the branch's final URL"
    );
    assert_eq!(
        eval_js(&b, &parent, "localStorage.getItem('cart')").await,
        json!("from_branch")
    );
    let cookie = eval_js(&b, &parent, "document.cookie").await;
    assert!(
        cookie.as_str().unwrap_or("").contains("bc=1"),
        "cookie not applied: {cookie}"
    );
    assert!(
        !target_ids(port).await.contains(&branch_tab),
        "branch tab must be closed after commit"
    );
    assert!(b.branch_commit("try_two").await.is_err(), "double commit");
    let listed = b.branch_list(None).await.unwrap();
    assert_eq!(listed["branches"][0]["status"], json!("committed"));
}

#[tokio::test(flavor = "multi_thread")]
async fn branch_discard_closes_the_tab_and_a_failed_commit_stays_active() {
    let Some((b, parent, port)) = launch().await else {
        return;
    };
    let (base, _stop) = serve(vec![("/p1", page("one"))]).await;
    goto(&b, &parent, &format!("{base}/p1")).await;

    // Discard really closes the target.
    let created = b.branch_create(&parent, "doomed").await.unwrap();
    let doomed_tab = created["branch_target_id"].as_str().unwrap().to_string();
    assert!(target_ids(port).await.contains(&doomed_tab));
    let discarded = b.branch_discard("doomed").await.expect("discard");
    assert_eq!(discarded["discarded"], json!(true));
    assert!(
        !target_ids(port).await.contains(&doomed_tab),
        "discarded branch tab must be gone from the target list"
    );
    assert!(parent_still_open(&b, &parent).await);
    assert!(b.branch_discard("doomed").await.is_err(), "double discard");

    // A commit that cannot read the branch must fail, not report success,
    // and must leave the branch active so it can still be cleaned up.
    let created = b.branch_create(&parent, "orphan").await.unwrap();
    let orphan_tab = created["branch_target_id"].as_str().unwrap().to_string();
    close_out_of_band(port, &orphan_tab).await;
    common::wait_until(
        "the orphaned tab to close",
        Duration::from_secs(2),
        || async { !target_ids(port).await.contains(&orphan_tab) },
    )
    .await;
    let err = b
        .branch_commit("orphan")
        .await
        .expect_err("commit of a dead branch must fail");
    assert!(!err_text(&err).is_empty());
    let listed = b.branch_list(Some(&orphan_tab)).await.unwrap();
    assert_eq!(listed["branches"][0]["status"], json!("active"), "{listed}");
    // Discard still works and tidies the leftover context.
    b.branch_discard("orphan").await.expect("discard orphan");
}

async fn parent_still_open(b: &CdpBackend, parent: &str) -> bool {
    b.eval(parent, "1 + 1").await.is_ok()
}

#[tokio::test(flavor = "multi_thread")]
async fn branch_cap_is_enforced_and_freed_by_discard() {
    if !have_chrome() {
        return;
    }
    let b = CdpBackend::new(NavPolicy::new(&[], true)).with_max_branches(2);
    let conn = b
        .connect(None, Some(json!({ "headless": true, "port": 0 })))
        .await
        .expect("launch");
    let port = conn["port"].as_u64().expect("connect reports the port");
    let parent = first_tab(&b, 1).await.expect("tab");

    b.branch_create(&parent, "a").await.expect("a");
    b.branch_create(&parent, "b").await.expect("b");
    let before = target_ids(port).await.len();
    let err = b
        .branch_create(&parent, "c")
        .await
        .expect_err("third branch must be refused");
    assert!(
        err_text(&err).contains("too many active branches"),
        "{}",
        err_text(&err)
    );
    assert_eq!(
        target_ids(port).await.len(),
        before,
        "a refused branch must not open a tab"
    );
    b.branch_discard("a").await.expect("discard a");
    b.branch_create(&parent, "c")
        .await
        .expect("a slot is free again");
}

// --- checkpoints ------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn checkpoint_rollback_restores_url_form_state_storage_and_cookies() {
    let Some((b, tab, _)) = launch().await else {
        return;
    };
    let form = page(
        r#"<input id="name" value="orig">
           <input id="chk" type="checkbox">
           <select id="sel"><option>one</option><option>two</option></select>
           <input id="upload" type="file">"#,
    );
    let (base, stop) = serve(vec![("/a", form), ("/b", page("other"))]).await;
    let (a, bb) = (format!("{base}/a"), format!("{base}/b"));
    goto(&b, &tab, &a).await;
    eval_js(
        &b,
        &tab,
        "document.getElementById('name').value='Alice';
         document.getElementById('chk').checked=true;
         document.getElementById('sel').selectedIndex=1;
         localStorage.setItem('k','v');
         document.cookie='ck=1; path=/'; true",
    )
    .await;
    let saved = b.checkpoint_save(&tab, Some("a")).await.expect("save");
    assert_eq!(saved["inputs_captured"], json!(3), "file input is skipped");

    // Move away and wreck the state.
    goto(&b, &tab, &bb).await;
    eval_js(
        &b,
        &tab,
        "localStorage.clear();
         document.cookie='ck=; expires=Thu, 01 Jan 1970 00:00:00 GMT; path=/'; true",
    )
    .await;
    assert_eq!(
        eval_js(&b, &tab, "document.cookie").await,
        json!(""),
        "cookie wiped"
    );

    let rolled = b
        .checkpoint_rollback(&tab, Some("a"))
        .await
        .expect("rollback");
    assert_eq!(rolled["rolled_back"], json!(true), "{rolled}");
    assert_eq!(rolled["navigated"], json!(true));
    assert_eq!(rolled["restored_inputs"], json!(3));
    assert_eq!(eval_js(&b, &tab, "location.href").await, json!(a));
    assert_eq!(
        eval_js(&b, &tab, "document.getElementById('name').value").await,
        json!("Alice")
    );
    assert_eq!(
        eval_js(&b, &tab, "document.getElementById('chk').checked").await,
        json!(true)
    );
    assert_eq!(
        eval_js(&b, &tab, "document.getElementById('sel').selectedIndex").await,
        json!(1)
    );
    assert_eq!(
        eval_js(&b, &tab, "localStorage.getItem('k')").await,
        json!("v")
    );
    let cookie = eval_js(&b, &tab, "document.cookie").await;
    assert!(cookie.as_str().unwrap_or("").contains("ck=1"), "{cookie}");

    // A rollback that cannot reach the checkpoint's page must say so. The
    // page lives on its own server so it can be taken away.
    let (gone_base, gone_stop) = serve(vec![("/z", page("z"))]).await;
    let z = format!("{gone_base}/z");
    goto(&b, &tab, &z).await;
    b.checkpoint_save(&tab, Some("z")).await.expect("save z");
    goto(&b, &tab, &bb).await;
    let _ = gone_stop.send(());
    let host = gone_base.trim_start_matches("http://").to_string();
    common::wait_until(
        "the fixture server to stop accepting",
        Duration::from_secs(5),
        || async { TcpStream::connect(host.as_str()).await.is_err() },
    )
    .await;
    assert!(
        TcpStream::connect(host.as_str()).await.is_err(),
        "fixture server should be down"
    );
    let err = b
        .checkpoint_rollback(&tab, Some("z"))
        .await
        .expect_err("rollback to an unreachable page must fail, not claim success");
    assert!(
        err_text(&err).to_lowercase().contains("navigate"),
        "{}",
        err_text(&err)
    );
    drop(stop);
}

// --- within scoping ---------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn act_within_scopes_to_the_row_and_never_widens() {
    let Some((b, tab, _)) = launch().await else {
        return;
    };
    let table = page(
        r#"<table><tbody>
             <tr id="r1"><td>Alpha</td><td><button onclick="window.clicked='row1'">Dispense</button></td></tr>
             <tr id="r2"><td>Beta</td><td><button onclick="window.clicked='row2'">Dispense</button></td></tr>
           </tbody></table>"#,
    );
    let (base, _stop) = serve(vec![("/t", table)]).await;
    goto(&b, &tab, &format!("{base}/t")).await;

    let click = |by: &'static str, query: &'static str, within: Option<&'static str>| {
        let b = &b;
        let tab = &tab;
        async move {
            eval_js(b, tab, "window.clicked = null; true").await;
            let r = b
                .act(
                    tab,
                    Locator::Selector {
                        by,
                        query,
                        within,
                        text: Some("Dispense"),
                        index: None,
                    },
                    "click",
                    None,
                )
                .await;
            let clicked = eval_js(b, tab, "window.clicked").await;
            (r, clicked)
        }
    };

    // CSS within.
    let (r, clicked) = click("css", "button", Some("tbody tr:nth-child(2)")).await;
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(clicked, json!("row2"));
    // XPath within used to throw inside the CSS try block and widen to the
    // whole document, clicking row 1's button for a row 2 request.
    let (r, clicked) = click("css", "button", Some("//tbody/tr[2]")).await;
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(clicked, json!("row2"));
    let (r, clicked) = click("css", "button", Some("(//tbody/tr)[1]")).await;
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(clicked, json!("row1"));
    // XPath query relative to the root, and absolute `//` made relative.
    let (r, clicked) = click("xpath", ".//button", Some("#r2")).await;
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(clicked, json!("row2"));
    let (r, clicked) = click("xpath", "//button", Some("#r2")).await;
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(
        clicked,
        json!("row2"),
        "absolute // must stay inside within"
    );

    // A missing root is an error, and nothing is clicked.
    for bad in ["#nope", "//tbody/tr[9]"] {
        let (r, clicked) = click("css", "button", Some(bad)).await;
        let e = r.expect_err("missing within root must not widen the search");
        assert!(matches!(e, BrowserError::NotFound(_)), "{e:?}");
        assert!(err_text(&e).contains("not found"), "{}", err_text(&e));
        assert_eq!(clicked, Value::Null, "{bad} widened to the whole page");
    }
    // An unparseable root is an error too.
    let (r, clicked) = click("css", "button", Some("tr[[")).await;
    assert!(r.is_err());
    assert_eq!(clicked, Value::Null);
    // An absolute single-slash XPath cannot be scoped, so it is refused.
    let (r, clicked) = click("xpath", "/html/body//button", Some("#r2")).await;
    let e = r.expect_err("absolute xpath with within must be refused");
    assert!(err_text(&e).contains("relative"), "{}", err_text(&e));
    assert_eq!(clicked, Value::Null);
}

// --- htmx -------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn htmx_settled_waits_for_requests_and_settle_events() {
    let Some((b, tab, _)) = launch().await else {
        return;
    };
    let fake = page(
        r#"<div id="t"></div>
<script>
  window.htmx = { version: 'fake' };
  window.__log = [];
  function ev(n) {
    document.body.dispatchEvent(new CustomEvent('htmx:' + n, { bubbles: true }));
    window.__log.push(n);
  }
  // Mirrors real htmx: classes on the element while a request/swap is live.
  window.__withClasses = function () {
    var t = document.getElementById('t');
    t.classList.add('htmx-request'); ev('beforeRequest');
    setTimeout(function () { t.classList.remove('htmx-request'); t.classList.add('htmx-settling'); ev('afterRequest'); }, 300);
    setTimeout(function () { t.classList.remove('htmx-settling'); ev('afterSettle'); }, 600);
  };
  // Events only: nothing but the in-flight counter can see this request.
  window.__eventsOnly = function () {
    ev('beforeRequest');
    setTimeout(function () { ev('afterRequest'); }, 400);
    setTimeout(function () { ev('afterSettle'); }, 450);
  };
</script>"#,
    );
    let (base, _stop) = serve(vec![("/h", fake), ("/plain", page("no htmx here"))]).await;

    // Without htmx there is nothing to wait for: an error, not "settled".
    goto(&b, &tab, &format!("{base}/plain")).await;
    let err = b
        .wait(&tab, "htmx_settled", None, 1500)
        .await
        .expect_err("htmx absent must not report settled");
    assert!(
        err_text(&err).contains("htmx not present"),
        "{}",
        err_text(&err)
    );

    goto(&b, &tab, &format!("{base}/h")).await;
    // Idle page: settles (and installs the listeners).
    b.wait(&tab, "htmx_settled", None, 3000)
        .await
        .expect("idle page settles");

    eval_js(&b, &tab, "window.__eventsOnly(); true").await;
    let t0 = std::time::Instant::now();
    b.wait(&tab, "htmx_settled", None, 5000)
        .await
        .expect("events-only request settles");
    assert!(
        t0.elapsed() >= std::time::Duration::from_millis(350),
        "returned after {:?}, before afterRequest",
        t0.elapsed()
    );
    let log = eval_js(&b, &tab, "window.__log.join(',')").await;
    assert!(
        log.as_str().unwrap_or("").ends_with("afterSettle"),
        "returned before afterSettle: {log}"
    );

    eval_js(
        &b,
        &tab,
        "window.__log.length = 0; window.__withClasses(); true",
    )
    .await;
    let t1 = std::time::Instant::now();
    b.wait(&tab, "htmx_settled", None, 5000)
        .await
        .expect("class-driven request settles");
    assert!(
        t1.elapsed() >= std::time::Duration::from_millis(500),
        "returned after {:?}, before afterSettle",
        t1.elapsed()
    );
    assert_eq!(
        eval_js(&b, &tab, "window.__log.join(',')").await,
        json!("beforeRequest,afterRequest,afterSettle")
    );
    assert_eq!(
        eval_js(&b, &tab, "document.getElementById('t').className").await,
        json!("")
    );
}

// --- profile auto-connect ---------------------------------------------------

fn call_ctx() -> CallCtx {
    CallCtx::new("state-truthfulness", CancelToken::new())
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_with_profile_restores_storage_on_the_saved_page() {
    if !have_chrome() {
        return;
    }
    let backend = Arc::new(CdpBackend::new(NavPolicy::new(&[], true)));
    let mut store_path = std::env::temp_dir();
    store_path.push(format!(
        "agentctl-truthful-profiles-{}.json",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&store_path);
    let module = BrowserModule::new(backend.clone())
        .with_profile_store(ProfileStore::new(store_path.clone(), 10));

    backend
        .connect(None, Some(json!({ "headless": true, "port": 0 })))
        .await
        .expect("launch source browser");
    let src = first_tab(&backend, 1).await.expect("source tab");
    let (base, _stop) = serve(vec![("/app", page("app"))]).await;
    let app = format!("{base}/app");
    goto(&backend, &src, &app).await;
    eval_js(&backend, &src, "localStorage.setItem('tok','abc'); true").await;
    let saved = module
        .call(
            "browser_profile",
            json!({ "action": "save", "name": "logged_in", "target_id": src }),
            &call_ctx(),
        )
        .await;
    assert!(saved.ok, "{saved:?}");

    // An unknown profile is an error and must not launch a browser.
    let unknown = module
        .call(
            "browser_connect",
            json!({ "launch": { "headless": true, "port": 0 }, "profile": "nope" }),
            &call_ctx(),
        )
        .await;
    assert!(!unknown.ok, "{unknown:?}");
    assert_eq!(unknown.error.as_ref().unwrap().code, ErrorCode::NotFound);

    // A fresh browser has empty storage; connecting with the profile must
    // load the saved page first and only then write storage into it.
    let conn = module
        .call(
            "browser_connect",
            json!({ "launch": { "headless": true, "port": 0 }, "profile": "logged_in" }),
            &call_ctx(),
        )
        .await;
    assert!(conn.ok, "{conn:?}");
    let data = conn.data.unwrap();
    assert_eq!(data["profile_restored"], json!("logged_in"), "{data}");
    let id = data["browser_id"].as_u64().unwrap() as u32;
    let tab2 = first_tab(&backend, id).await.expect("restored tab");
    assert_eq!(eval_js(&backend, &tab2, "location.href").await, json!(app));
    assert_eq!(
        eval_js(&backend, &tab2, "localStorage.getItem('tok')").await,
        json!("abc")
    );
    let _ = std::fs::remove_file(&store_path);
}

/// Shutdown closes branch tabs in a browser we only attached to, which
/// outlives us and would otherwise keep them forever.
#[tokio::test]
async fn dropping_an_attached_backend_closes_its_branches() {
    // The owner launches and keeps the browser alive.
    let Some((owner, _, port)) = launch().await else {
        return;
    };
    let (base, _stop) = serve(vec![("/p", page("p"))]).await;

    let attached = CdpBackend::new(NavPolicy::new(&[], true));
    attached
        .connect(Some(port as u16), None)
        .await
        .expect("attach to the owner's browser");
    let parent = first_tab(&attached, 1).await.expect("tab");
    goto(&attached, &parent, &format!("{base}/p")).await;
    let mut branch_tabs = Vec::new();
    for id in ["a", "b", "c"] {
        let created = attached.branch_create(&parent, id).await.expect("create");
        branch_tabs.push(created["branch_target_id"].as_str().unwrap().to_string());
    }
    let open = target_ids(port).await;
    assert!(branch_tabs.iter().all(|t| open.contains(t)), "{open:?}");

    drop(attached);

    let open = target_ids(port).await;
    assert!(
        branch_tabs.iter().all(|t| !open.contains(t)),
        "branch tabs survived shutdown: {open:?}"
    );
    drop(owner);
}
