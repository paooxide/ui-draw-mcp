//! Live tests (real headless Chrome) for the three ways a locator reaches an
//! element that CSS on the light DOM does not: by ARIA role (and accessible
//! name), through an open shadow root, and into a same-origin iframe.
//!
//! The benchmarks that motivate it (WebArena, Online-Mind2Web) are written for
//! Playwright, so models send `role=button[name="Submit"]` and
//! `getByRole('button', { name: 'Submit' })`, and their pages put controls in
//! web components and embedded frames. A frame of another origin cannot be read:
//! it must be reported, not crash the call. Skipped when `AGENTCTL_SKIP_LIVE` is
//! set or no Chrome binary is found.

use std::sync::Arc;

use mcp_browser::{BrowserBackend, BrowserModule, CdpBackend, NavPolicy, CHROME_BINS};
use mcp_types::ToolModule;
use mcp_types::{CallCtx, CancelToken, Envelope};
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

/// Roles: decoys around each button we mean, and the implicit-role elements.
/// Every click logs what it was into `window.__hits`.
const ROLES: &str = r#"<!doctype html><body style="margin:20px">
<script>window.__hits = []; function hit(s){ window.__hits.push(s); }</script>
<button onclick="hit('cancel')">Cancel</button>
<button onclick="hit('submit-form')">Submit form</button>
<button onclick="hit('submit')">Submit</button>
<div role="button" tabindex="0" onclick="hit('aria')" aria-label="Submit later">later</div>
<a href="/docs" onclick="hit('docs'); return false">Docs</a>
<a onclick="hit('nohref')">Not a link</a>
<label for="email">Email address</label><input id="email">
<textarea placeholder="Your message"></textarea>
<input type="checkbox" id="agree" aria-label="Agree to terms">
<h1>Title</h1><h2>Section two</h2><h3>Sub three</h3>
<img src="data:image/gif;base64,R0lGODlhAQABAAAAACH5BAEKAAEALAAAAAABAAEAAAICTAEAOw==" alt="Logo">
<ul><li>one</li><li>two</li></ul>
<select aria-label="Pick"><option>A</option><option>B</option></select>
</body>"#;

/// An open shadow root holding a button and a field.
const SHADOW: &str = r#"<!doctype html><body style="margin:20px">
<script>window.__hits = []; function hit(s){ window.__hits.push(s); }</script>
<button id="light" onclick="hit('light')">Light Go</button>
<div id="host"></div>
<script>
var root = document.getElementById('host').attachShadow({mode: 'open'});
root.innerHTML = '<button id="sb">Shadow Go</button><input aria-label="Inner field">';
root.getElementById('sb').addEventListener('click', function(e){ hit('shadow:' + e.isTrusted); });
</script></body>"#;

/// Frames: a bordered, padded iframe (so its offset is not just its corner)
/// with a button and a field; that frame holds a nested one with a button; and
/// a frame of another port, which is another origin. `{OTHER}` is that port.
const FRAMES: &str = r#"<!doctype html><body style="margin:20px">
<script>window.__hits = []; function hit(s){ window.__hits.push(s); }</script>
<iframe id="f1" src="/frame" width="420" height="260" style="border:4px solid #444; padding:6px; margin-left:30px"></iframe>
<iframe id="alien" src="http://127.0.0.1:{OTHER}/alien" width="200" height="60"></iframe>
</body>"#;

const FRAME: &str = r#"<!doctype html><body style="margin:8px">
<button id="fb">Frame Go</button>
<input id="fi" aria-label="Frame field">
<iframe id="f2" src="/inner" width="300" height="80"></iframe>
<script>
document.getElementById('fb').addEventListener('click', function(e){ parent.hit('frame:' + e.isTrusted); });
document.getElementById('fi').addEventListener('input', function(e){ parent.hit('typed:' + e.isTrusted + ':' + e.target.value); });
</script></body>"#;

const INNER: &str = r#"<!doctype html><body style="margin:4px">
<button id="db">Deep Go</button>
<script>document.getElementById('db').addEventListener('click', function(e){ parent.parent.hit('deep:' + e.isTrusted); });</script>
</body>"#;

const ALIEN: &str = r#"<!doctype html><body><button id="ab">Alien Go</button></body>"#;

/// Serve `pages` (path, body) on a fresh port; `{OTHER}` in a body is replaced
/// with `other`. Unknown paths are 404.
async fn serve(
    pages: &'static [(&'static str, &'static str)],
    other: u16,
) -> (u16, tokio::sync::oneshot::Sender<()>) {
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
                        let mut buf = [0u8; 4096];
                        let n = stream.read(&mut buf).await.unwrap_or(0);
                        let req = String::from_utf8_lossy(&buf[..n]);
                        let path = req.split_whitespace().nth(1).unwrap_or("/");
                        let hit = pages.iter().find(|(p, _)| *p == path);
                        let (status, body) = match hit {
                            Some((_, b)) => ("200 OK", b.replace("{OTHER}", &other.to_string())),
                            None => ("404 Not Found", String::new()),
                        };
                        let resp = format!(
                            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = stream.write_all(resp.as_bytes()).await;
                        let _ = stream.flush().await;
                    });
                }
            }
        }
    });
    (port, tx)
}

fn ctx() -> CallCtx {
    CallCtx::new("test", CancelToken::new())
}

async fn page(b: &CdpBackend, t: &str, js: &str) -> String {
    b.eval(t, js).await.expect("eval")["result"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// `window.__hits` as one string, cleared on read.
async fn hits(b: &CdpBackend, t: &str) -> String {
    page(
        b,
        t,
        "(function(){ var h = window.__hits.join(','); window.__hits.length = 0; return h; })()",
    )
    .await
}

type Setup = (
    Arc<CdpBackend>,
    String,
    BrowserModule,
    Vec<tokio::sync::oneshot::Sender<()>>,
);

/// Open `/` of a server with `pages`, and a second server (the other origin)
/// serving the alien page.
async fn setup(pages: &'static [(&'static str, &'static str)]) -> Option<Setup> {
    let (b, t) = tab().await?;
    let (other, stop_other) = serve(&[("/alien", ALIEN)], 0).await;
    let (port, stop) = serve(pages, other).await;
    b.navigate(&t, "goto", Some(&format!("http://127.0.0.1:{port}/")))
        .await
        .expect("goto");
    let m = BrowserModule::new(b.clone());
    Some((b, t, m, vec![stop, stop_other]))
}

async fn finish(b: &CdpBackend, stops: Vec<tokio::sync::oneshot::Sender<()>>) {
    for s in stops {
        let _ = s.send(());
    }
    let _ = b.disconnect(1, true).await;
}

async fn call(m: &BrowserModule, tool: &str, args: Value) -> Envelope {
    m.call(tool, args, &ctx()).await
}

fn err(e: &Envelope) -> String {
    e.error.as_ref().expect("error").message.clone()
}

/// `browser_act` on `query` as given (no `by`, so the spelling decides).
async fn click(m: &BrowserModule, t: &str, query: &str) -> Envelope {
    call(
        m,
        "browser_act",
        json!({ "target_id": t, "query": query, "action": "click" }),
    )
    .await
}

/// The first match of `query`, as browser_query returns it.
async fn first(m: &BrowserModule, t: &str, args: Value) -> Value {
    let mut a = json!({ "target_id": t });
    for (k, v) in args.as_object().expect("object") {
        a[k] = v.clone();
    }
    let r = call(m, "browser_query", a).await;
    assert!(r.ok, "{r:?}");
    r.data.unwrap()["matches"][0].clone()
}

/// Every spelling of "the Submit button" clicks it, not its decoys.
#[tokio::test(flavor = "multi_thread")]
async fn every_role_spelling_clicks_the_right_button() {
    let Some((b, t, m, stops)) = setup(&[("/", ROLES)]).await else {
        return;
    };
    let spellings = [
        r#"role=button[name="Submit"]"#,
        "role=button[name=Submit]",
        "role=button[name='Submit']",
        r#"role=button[name=/^submit$/i]"#,
        "getByRole('button', { name: 'Submit' })",
        r#"page.getByRole("button", {name: "Submit", exact: true})"#,
        r#"screen.getByRole('button', { name: /^Submit$/i })"#,
        r#"button "Submit""#,
        r#"- button "Submit" [ref=e4]"#,
    ];
    for q in spellings {
        let r = click(&m, &t, q).await;
        assert!(r.ok, "{q}: {r:?}");
        assert_eq!(hits(&b, &t).await, "submit", "{q}");
    }

    // `by: role` with the role as the query and `name` beside it; the name is
    // a substring, so it finds "Cancel" by "ancel" and, given both Submit
    // buttons, prefers the exact one.
    let r = call(
        &m,
        "browser_act",
        json!({ "target_id": t, "by": "role", "query": "button", "name": "Submit", "action": "click" }),
    )
    .await;
    assert!(r.ok, "{r:?}");
    assert_eq!(hits(&b, &t).await, "submit");
    let r = call(
        &m,
        "browser_act",
        json!({ "target_id": t, "by": "role", "query": "button", "name": "ancel", "action": "click" }),
    )
    .await;
    assert!(r.ok, "{r:?}");
    assert_eq!(hits(&b, &t).await, "cancel");

    // An aria-label is the name of a div[role=button]; every button with
    // "submit" in its name matches the substring spelling, in page order.
    let all = call(
        &m,
        "browser_query",
        json!({ "target_id": t, "by": "role", "query": "button", "name": "later", "all": true }),
    )
    .await;
    assert_eq!(all.data.unwrap()["count"], 1);
    let r = click(&m, &t, r#"role=button[name=/submit/i]"#).await;
    assert!(r.ok, "{r:?}");
    assert_eq!(hits(&b, &t).await, "submit-form");
    finish(&b, stops).await;
}

/// Implicit roles: link needs an href, textbox covers inputs and textareas,
/// checkbox, heading and its level, img, list items, a select.
#[tokio::test(flavor = "multi_thread")]
async fn implicit_roles_and_accessible_names() {
    let Some((b, t, m, stops)) = setup(&[("/", ROLES)]).await else {
        return;
    };
    let count = |args: Value| {
        let (m, t) = (&m, &t);
        async move {
            let mut a = json!({ "target_id": t, "all": true });
            for (k, v) in args.as_object().unwrap() {
                a[k] = v.clone();
            }
            let r = call(m, "browser_query", a).await;
            assert!(r.ok, "{r:?}");
            r.data.unwrap()["count"].as_u64().unwrap()
        }
    };
    // <a> without href is not a link.
    assert_eq!(count(json!({ "by": "role", "query": "link" })).await, 1);
    assert_eq!(count(json!({ "query": "role=link[name=docs]" })).await, 1);
    assert_eq!(
        count(json!({ "query": "role=link[name=\"Not a link\"]" })).await,
        0
    );
    // textbox: the input named by its <label>, the textarea by its placeholder.
    assert_eq!(count(json!({ "by": "role", "query": "textbox" })).await, 2);
    assert_eq!(
        count(json!({ "by": "role", "query": "textbox", "name": "Email address" })).await,
        1
    );
    assert_eq!(
        count(json!({ "query": "getByRole('textbox', { name: 'message' })" })).await,
        1
    );
    assert_eq!(
        count(json!({ "by": "role", "query": "checkbox", "name": "terms" })).await,
        1
    );
    assert_eq!(count(json!({ "by": "role", "query": "heading" })).await, 3);
    assert_eq!(count(json!({ "query": "role=heading[level=2]" })).await, 1);
    assert_eq!(
        count(json!({ "query": "getByRole('heading', { level: 3 })" })).await,
        1
    );
    assert_eq!(
        count(json!({ "by": "role", "query": "img", "name": "logo" })).await,
        1
    );
    assert_eq!(count(json!({ "by": "role", "query": "listitem" })).await, 2);
    assert_eq!(count(json!({ "by": "role", "query": "list" })).await, 1);
    assert_eq!(
        count(json!({ "by": "role", "query": "combobox", "name": "Pick" })).await,
        1
    );

    let h = first(&m, &t, json!({ "query": "role=heading[level=2]" })).await;
    assert_eq!(h["name"], "Section two", "{h}");

    // Type into the labelled input and tick the checkbox.
    let typed = call(
        &m,
        "browser_act",
        json!({ "target_id": t, "query": r#"role=textbox[name="Email"]"#, "action": "type", "value": "a@b.c" }),
    )
    .await;
    assert!(typed.ok, "{typed:?}");
    assert_eq!(typed.data.unwrap()["value_after"], "a@b.c");
    let tick = click(&m, &t, "getByRole('checkbox', { name: 'Agree to terms' })").await;
    assert!(tick.ok, "{tick:?}");
    assert_eq!(
        page(&b, &t, "String(document.getElementById('agree').checked)").await,
        "true"
    );
    finish(&b, stops).await;
}

/// A role miss says what it looked for and lists the elements that have the
/// role, nearest name first.
#[tokio::test(flavor = "multi_thread")]
async fn a_role_miss_lists_the_nearest_elements_with_that_role() {
    let Some((b, t, m, stops)) = setup(&[("/", ROLES)]).await else {
        return;
    };
    let r = click(&m, &t, r#"role=button[name="Sbumit"]"#).await;
    assert!(!r.ok, "{r:?}");
    let msg = err(&r);
    assert!(
        msg.contains("no element has the role button named \"Sbumit\""),
        "{msg}"
    );
    assert!(msg.contains("button \"Cancel\""), "{msg}");
    assert!(msg.contains("button \"Submit\""), "{msg}");
    assert!(msg.contains("button \"Submit form\""), "{msg}");

    // A word in common comes first.
    let r = click(&m, &t, r#"role=button[name="Submit now"]"#).await;
    let msg = err(&r);
    let cancel = msg.find("\"Cancel\"").expect(&msg);
    assert!(msg.find("\"Submit\"").expect(&msg) < cancel, "{msg}");

    // A role nothing has says which roles the page does have.
    let r = click(&m, &t, "getByRole('tab', { name: 'Home' })").await;
    let msg = err(&r);
    assert!(msg.contains("none with that role"), "{msg}");
    assert!(
        msg.contains("roles on the page:") && msg.contains("button"),
        "{msg}"
    );
    finish(&b, stops).await;
}

/// A button in an open shadow root is found by text, CSS and role, and
/// clicked; its ref resolves again on the next call and the snapshot lists it.
#[tokio::test(flavor = "multi_thread")]
async fn shadow_dom_is_searched_and_its_refs_resolve() {
    let Some((b, t, m, stops)) = setup(&[("/", SHADOW)]).await else {
        return;
    };
    for q in [
        json!({ "query": "Shadow Go" }),
        json!({ "by": "text", "query": "shadow go" }),
        json!({ "by": "css", "query": "#sb" }),
        json!({ "query": "button#sb" }),
        json!({ "by": "role", "query": "button", "name": "Shadow Go" }),
        json!({ "query": "getByRole('button', { name: 'Shadow Go' })" }),
    ] {
        let node = first(&m, &t, q.clone()).await;
        assert!(
            node["ref"].as_str().unwrap().contains("::shadow/"),
            "{q}: {node}"
        );
        let mut a = json!({ "target_id": t, "action": "click" });
        for (k, v) in q.as_object().unwrap() {
            a[k] = v.clone();
        }
        let r = call(&m, "browser_act", a).await;
        assert!(r.ok, "{q}: {r:?}");
        assert_eq!(hits(&b, &t).await, "shadow:true", "{q}");
    }

    // The ref from one call drives the next.
    let node = first(
        &m,
        &t,
        json!({ "by": "role", "query": "button", "name": "Shadow Go" }),
    )
    .await;
    let r = call(
        &m,
        "browser_act",
        json!({ "target_id": t, "ref": node["ref"], "action": "click" }),
    )
    .await;
    assert!(r.ok, "{r:?}");
    assert_eq!(r.data.as_ref().unwrap()["input"], "cdp", "{r:?}");
    assert_eq!(hits(&b, &t).await, "shadow:true");
    let field = first(
        &m,
        &t,
        json!({ "by": "role", "query": "textbox", "name": "Inner field" }),
    )
    .await;
    let typed = call(
        &m,
        "browser_act",
        json!({ "target_id": t, "ref": field["ref"], "action": "type", "value": "hello" }),
    )
    .await;
    assert!(typed.ok, "{typed:?}");
    assert_eq!(typed.data.unwrap()["value_after"], "hello");

    // Light DOM refs keep their old shape.
    let light = first(&m, &t, json!({ "query": "#light" })).await;
    assert_eq!(light["ref"], "//*[@id=\"light\"]");

    let snap = call(&m, "browser_snapshot", json!({ "target_id": t })).await;
    assert!(snap.ok, "{snap:?}");
    let nodes = snap.data.unwrap()["nodes"].as_array().unwrap().clone();
    let inner = nodes
        .iter()
        .find(|n| n["name"] == "Shadow Go")
        .unwrap_or_else(|| panic!("not in the snapshot: {nodes:?}"));
    assert!(inner["ref"].as_str().unwrap().contains("::shadow/"));
    assert!(
        nodes.iter().any(|n| n["name"] == "Inner field"),
        "{nodes:?}"
    );
    finish(&b, stops).await;
}

/// Buttons and fields in same-origin frames, one nested, are found, typed into
/// and clicked with real input; the frame's offset is added to their boxes. A
/// frame of another origin is reported, and does not break the search.
#[tokio::test(flavor = "multi_thread")]
async fn same_origin_frames_are_searched_and_clicked_with_real_input() {
    let Some((b, t, m, stops)) =
        setup(&[("/", FRAMES), ("/frame", FRAME), ("/inner", INNER)]).await
    else {
        return;
    };
    // Give the nested frames a moment to load.
    for _ in 0..40 {
        let ready = page(
            &b,
            &t,
            "(function(){ try { var f = document.getElementById('f1').contentDocument.getElementById('f2'); return String(!!f.contentDocument.getElementById('db')); } catch(e) { return 'false'; } })()",
        )
        .await;
        if ready == "true" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let node = first(&m, &t, json!({ "query": "Frame Go" })).await;
    let fref = node["ref"].as_str().unwrap().to_string();
    assert!(fref.contains("::frame/"), "{fref}");

    // The reported box is in the page: the iframe's corner, its border and
    // padding, and the button's place inside.
    let expect = page(
        &b,
        &t,
        "(function(){ var f = document.getElementById('f1'), r = f.getBoundingClientRect(), cs = getComputedStyle(f), \
         e = f.contentDocument.getElementById('fb').getBoundingClientRect(); \
         return [Math.round(r.left + f.clientLeft + parseFloat(cs.paddingLeft) + e.left), \
                 Math.round(r.top + f.clientTop + parseFloat(cs.paddingTop) + e.top)].join(','); })()",
    )
    .await;
    assert_eq!(format!("{},{}", node["x"], node["y"]), expect, "{node}");
    assert!(node["x"].as_i64().unwrap() > 38, "{node}");

    for q in [
        json!({ "query": "Frame Go" }),
        json!({ "by": "css", "query": "#fb" }),
        json!({ "by": "role", "query": "button", "name": "Frame Go" }),
        json!({ "ref": fref }),
    ] {
        let mut a = json!({ "target_id": t, "action": "click" });
        for (k, v) in q.as_object().unwrap() {
            a[k] = v.clone();
        }
        let r = call(&m, "browser_act", a).await;
        assert!(r.ok, "{q}: {r:?}");
        assert_eq!(r.data.as_ref().unwrap()["input"], "cdp", "{q}: {r:?}");
        assert_eq!(hits(&b, &t).await, "frame:true", "{q}");
    }

    let typed = call(
        &m,
        "browser_act",
        json!({ "target_id": t, "query": "role=textbox[name=\"Frame field\"]", "action": "type", "value": "hi" }),
    )
    .await;
    assert!(typed.ok, "{typed:?}");
    let d = typed.data.unwrap();
    assert_eq!(d["input"], "cdp", "{d}");
    assert_eq!(d["value_after"], "hi", "{d}");
    assert_eq!(hits(&b, &t).await, "typed:true:hi");

    // The nested frame: its ref has two frame hops.
    let deep = first(&m, &t, json!({ "query": "Deep Go" })).await;
    assert_eq!(
        deep["ref"].as_str().unwrap().matches("::frame/").count(),
        2,
        "{deep}"
    );
    let r = call(
        &m,
        "browser_act",
        json!({ "target_id": t, "ref": deep["ref"], "action": "click" }),
    )
    .await;
    assert!(r.ok, "{r:?}");
    assert_eq!(r.data.as_ref().unwrap()["input"], "cdp", "{r:?}");
    assert_eq!(hits(&b, &t).await, "deep:true");

    // The snapshot lists frame content and names the frame it skipped.
    let snap = call(&m, "browser_snapshot", json!({ "target_id": t })).await;
    assert!(snap.ok, "{snap:?}");
    let data = snap.data.unwrap();
    let nodes = data["nodes"].as_array().unwrap();
    for name in ["Frame Go", "Frame field", "Deep Go"] {
        assert!(nodes.iter().any(|n| n["name"] == name), "{name}: {nodes:?}");
    }
    assert!(!nodes.iter().any(|n| n["name"] == "Alien Go"), "{nodes:?}");
    let skipped = data["frames_skipped"].as_array().expect("frames_skipped");
    assert_eq!(skipped.len(), 1, "{skipped:?}");
    assert!(
        skipped[0]["src"].as_str().unwrap().contains("/alien"),
        "{skipped:?}"
    );

    // The other origin's button is a miss that says why, not a crash.
    let miss = click(&m, &t, "Alien Go").await;
    assert!(!miss.ok);
    let msg = err(&miss);
    assert!(msg.contains("cross-origin frame"), "{msg}");
    assert!(msg.contains("/alien"), "{msg}");
    let miss = click(&m, &t, "role=button[name=\"Alien Go\"]").await;
    assert!(err(&miss).contains("cross-origin frame"), "{miss:?}");
    finish(&b, stops).await;
}
