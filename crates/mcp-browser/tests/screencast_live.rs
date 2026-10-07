//! Live tests (real headless Chrome) for `browser_screencast` and
//! `browser_screenshot save`.
//!
//! Skipped when `AGENTCTL_SKIP_LIVE` is set or no Chrome binary is found. The
//! mp4 checks also need ffmpeg and ffprobe on PATH and fall back to checking
//! the kept frames without them.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use mcp_browser::{BrowserBackend, BrowserModule, CdpBackend, NavPolicy, CHROME_BINS};
use mcp_types::{CallCtx, CancelToken, Envelope, ToolModule};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn skip_live() -> bool {
    std::env::var_os("AGENTCTL_SKIP_LIVE").is_some_and(|v| v != "0")
}

fn have_chrome() -> bool {
    !skip_live() && CHROME_BINS.iter().any(|p| Path::new(p).exists())
}

fn have_tool(name: &str) -> bool {
    std::process::Command::new(name)
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

fn ctx() -> CallCtx {
    CallCtx::new("test", CancelToken::new())
}

struct Rig {
    m: BrowserModule,
    b: Arc<CdpBackend>,
    target: String,
    base: String,
    media: PathBuf,
    stop: tokio::sync::oneshot::Sender<()>,
}

/// Two animated pages: every frame of either differs from the last.
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
                        let (name, hue) = if path.starts_with("/two") { ("two", 200) } else { ("one", 20) };
                        let body = format!(
                            "<!doctype html><body style=\"margin:0\"><h1 id=\"which\">{name}</h1>\
                             <script>var n=0;setInterval(function(){{n++;\
                             document.body.style.background='hsl('+(({hue}+n*7)%360)+',70%,60%)';\
                             document.getElementById('which').textContent='{name} '+n;}},30)</script></body>"
                        );
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

async fn rig(tag: &str) -> Option<Rig> {
    if !have_chrome() {
        return None;
    }
    let b = Arc::new(CdpBackend::new(NavPolicy::new(&[], true)));
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
    let (base, stop) = serve().await;
    b.navigate(&target, "goto", Some(&format!("{base}/")))
        .await
        .ok()?;
    let media = std::env::temp_dir().join(format!("agentctl-media-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&media);
    let m = BrowserModule::new(b.clone()).with_media_dir(media.clone());
    Some(Rig {
        m,
        b,
        target,
        base,
        media,
        stop,
    })
}

impl Rig {
    async fn call(&self, tool: &str, args: Value) -> Envelope {
        self.m.call(tool, args, &ctx()).await
    }

    async fn status(&self) -> Value {
        let s = self
            .call("browser_screencast", json!({ "action": "status" }))
            .await;
        assert!(s.ok, "{s:?}");
        s.data.unwrap()
    }

    async fn frames(&self) -> u64 {
        self.status().await["recordings"][0]["frames"]
            .as_u64()
            .unwrap_or(0)
    }

    async fn finish(self) {
        let _ = self.stop.send(());
        let _ = self.b.disconnect(1, true).await;
        let _ = std::fs::remove_dir_all(&self.media);
    }
}

fn probe_duration(mp4: &str) -> f64 {
    let out = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=nw=1:nk=1",
            mp4,
        ])
        .output()
        .expect("ffprobe");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("ffprobe gave {out:?}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn screenshot_save_writes_a_png_under_media_and_returns_no_image() {
    let Some(r) = rig("shot").await else {
        return;
    };
    let t = r.target.clone();
    let e = r
        .call(
            "browser_screenshot",
            json!({ "target_id": t, "save": true }),
        )
        .await;
    assert!(e.ok, "{e:?}");
    assert!(e.image.is_none(), "no image payload when saved");
    let d = e.data.clone().unwrap();
    let path = PathBuf::from(d["path"].as_str().expect("path"));
    assert!(path.starts_with(r.media.join("screenshots")), "{path:?}");
    let bytes = std::fs::read(&path).expect("file");
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    assert_eq!(d["bytes"].as_u64(), Some(bytes.len() as u64));
    let w = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
    let h = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
    assert!(w > 0 && h > 0);
    assert_eq!(d["width"].as_u64(), Some(w as u64));
    assert_eq!(d["height"].as_u64(), Some(h as u64));

    // The default is unchanged: inline image, nothing written.
    let before = std::fs::read_dir(r.media.join("screenshots"))
        .unwrap()
        .count();
    let e = r
        .call("browser_screenshot", json!({ "target_id": t }))
        .await;
    assert!(e.ok && e.image.is_some(), "{e:?}");
    // The inline result carries the image's real size, never 0x0.
    let d = e.data.clone().unwrap();
    assert_eq!(d["width"].as_u64(), Some(w as u64), "{d}");
    assert_eq!(d["height"].as_u64(), Some(h as u64), "{d}");
    let after = std::fs::read_dir(r.media.join("screenshots"))
        .unwrap()
        .count();
    assert_eq!(before, after);
    r.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn saving_without_a_media_dir_is_a_clear_error() {
    let b = Arc::new(CdpBackend::new(NavPolicy::new(&[], true)));
    let m = BrowserModule::new(b);
    for (tool, args) in [
        (
            "browser_screenshot",
            json!({ "target_id": "x", "save": true }),
        ),
        (
            "browser_screencast",
            json!({ "action": "start", "target_id": "x" }),
        ),
    ] {
        let e = m.call(tool, args, &ctx()).await;
        assert!(!e.ok, "{tool}: {e:?}");
        let msg = e.error.unwrap().message;
        assert!(msg.contains("media directory"), "{msg}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn screencast_records_through_a_navigation_and_encodes_an_mp4() {
    let Some(r) = rig("cast").await else {
        return;
    };
    let t = r.target.clone();
    let started = r
        .call(
            "browser_screencast",
            json!({ "action": "start", "target_id": t, "fps": 15 }),
        )
        .await;
    assert!(started.ok, "{started:?}");
    let sd = started.data.clone().unwrap();
    let id = sd["recording_id"].as_str().unwrap().to_string();
    assert!(PathBuf::from(sd["dir"].as_str().unwrap()).starts_with(r.media.join("screencasts")));

    // One recording per tab.
    let again = r
        .call(
            "browser_screencast",
            json!({ "action": "start", "target_id": t }),
        )
        .await;
    assert!(!again.ok, "second start must fail: {again:?}");

    common::wait_until("first frames", Duration::from_secs(10), || async {
        r.frames().await >= 8
    })
    .await;
    let mid = r.frames().await;
    let nav = r
        .call(
            "browser_navigate",
            json!({ "target_id": t, "action": "goto", "url": format!("{}/two", r.base) }),
        )
        .await;
    assert!(nav.ok, "{nav:?}");
    common::wait_until(
        "frames after the navigation",
        Duration::from_secs(10),
        || async { r.frames().await >= mid + 8 },
    )
    .await;

    let stopped = r
        .call(
            "browser_screencast",
            json!({ "action": "stop", "recording_id": id }),
        )
        .await;
    assert!(stopped.ok, "{stopped:?}");
    let d = stopped.data.clone().unwrap();
    let frames = d["frames"].as_u64().unwrap();
    assert!(frames >= 16, "frames: {d}");
    let dur = d["duration_s"].as_f64().unwrap();
    assert!(dur >= 1.0, "duration: {d}");
    let dir = PathBuf::from(d["dir"].as_str().unwrap());

    if have_tool("ffmpeg") && have_tool("ffprobe") {
        assert_eq!(d["ffmpeg"], "ok", "{d}");
        let mp4 = d["path"].as_str().expect("mp4 path");
        assert!(Path::new(mp4).starts_with(&dir));
        let probed = probe_duration(mp4);
        eprintln!("frames={frames} duration_s={dur} ffprobe={probed}");
        assert!(
            (probed - dur).abs() < 0.3,
            "ffprobe {probed} vs reported {dur}"
        );
        assert!(!dir.join("frames").exists(), "frames removed after encode");
    } else {
        assert_eq!(d["ffmpeg"], "missing");
        assert!(d["note"].as_str().unwrap().contains("ffmpeg"));
        assert!(dir.join("frames.ffconcat").exists());
    }

    // The recording is gone; stopping again is not found.
    let e = r
        .call(
            "browser_screencast",
            json!({ "action": "stop", "target_id": t }),
        )
        .await;
    assert!(!e.ok);
    r.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn keep_frames_auto_stop_and_disconnect_cleanup() {
    let Some(r) = rig("keep").await else {
        return;
    };
    let t = r.target.clone();
    let s = r
        .call(
            "browser_screencast",
            json!({ "action": "start", "target_id": t, "fps": 10, "max_seconds": 1 }),
        )
        .await;
    assert!(s.ok, "{s:?}");
    // max_seconds ends the capture by itself; stop still reports it.
    common::wait_until("auto stop", Duration::from_secs(10), || async {
        r.status().await["recordings"][0]["running"] == false
    })
    .await;
    let d = r
        .call(
            "browser_screencast",
            json!({ "action": "stop", "target_id": t, "keep_frames": true }),
        )
        .await
        .data
        .unwrap();
    assert_eq!(d["ended"], "max_seconds", "{d}");
    let dir = PathBuf::from(d["dir"].as_str().unwrap());
    assert!(dir.join("frames.ffconcat").exists());
    assert!(dir.join("frames/00001.jpg").exists());

    // A recording does not outlive its browser.
    let s = r
        .call(
            "browser_screencast",
            json!({ "action": "start", "target_id": t }),
        )
        .await;
    assert!(s.ok, "{s:?}");
    r.b.disconnect(1, true).await.expect("disconnect");
    assert_eq!(r.status().await["recordings"], json!([]));
    r.finish().await;
}
