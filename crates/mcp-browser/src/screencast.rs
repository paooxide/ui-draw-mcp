//! Video capture of a tab (`browser_screencast`) and screenshots saved to disk.
//!
//! A recording polls `Page.captureScreenshot` (JPEG) on its own CDP session
//! and keeps real timestamps, then `stop` turns the frames into an H.264 mp4
//! with ffmpeg's concat demuxer (variable frame rate). `Page.startScreencast`
//! is not used: it delivers nothing while the window is occluded or
//! throttled, whereas a surface capture plus focus emulation keeps working.
//!
//! Files are written only under an agentctl-owned media directory, with names
//! this module generates; no caller-chosen path ever reaches the filesystem.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::backend::BrowserError;
use crate::cdp::{http_json, CdpConn};

/// Validated options for a recording (see [`ScreencastOpts::from_args`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreencastOpts {
    pub fps: u32,
    pub quality: u32,
    pub max_seconds: u32,
}

impl ScreencastOpts {
    pub fn from_args(fps: Option<i64>, quality: Option<i64>, max_seconds: Option<i64>) -> Self {
        ScreencastOpts {
            fps: clamp_or(fps, 15, 1, 30),
            quality: clamp_or(quality, 80, 30, 95),
            max_seconds: clamp_or(max_seconds, 300, 1, 1800),
        }
    }

    /// Most frames a recording may hold, whatever the clock says.
    pub fn max_frames(&self) -> usize {
        self.fps as usize * self.max_seconds as usize
    }
}

fn clamp_or(v: Option<i64>, default: u32, min: u32, max: u32) -> u32 {
    v.map_or(default, |n| n.clamp(min as i64, max as i64) as u32)
}

/// How long a capture may fail in a row before the recording gives up.
const GIVE_UP: Duration = Duration::from_secs(5);
/// Wait between reconnect attempts while the page is unreachable.
const RETRY_EVERY: Duration = Duration::from_millis(500);
/// Longest `stop` waits for the capture task to wind down before aborting it.
const STOP_GRACE: Duration = Duration::from_secs(8);
/// Longest the encoder may run.
const FFMPEG_TIMEOUT: Duration = Duration::from_secs(300);

/// A unique, filesystem-safe stem: unix milliseconds plus a per-process
/// counter, so two names made in the same millisecond differ.
pub fn media_name(unix_ms: u128, counter: u64) -> String {
    format!("{unix_ms:013}-{counter:04x}")
}

fn next_media_name() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(1);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    media_name(ms, SEQ.fetch_add(1, Ordering::Relaxed))
}

/// File name of the `n`th frame (1-based).
pub fn frame_name(n: usize) -> String {
    format!("{n:05}.jpg")
}

/// Text of an ffconcat script for frames given as (file, seconds since the
/// recording began). Each frame lasts until the next one; the last lasts
/// `last_duration`. ffmpeg ignores the final `duration` unless the last file
/// is repeated, hence the closing `file` line.
pub fn ffconcat(frames: &[(String, f64)], last_duration: f64) -> String {
    let mut out = String::from("ffconcat version 1.0\n");
    for (i, (name, t)) in frames.iter().enumerate() {
        let d = match frames.get(i + 1) {
            Some((_, next)) => (next - t).max(0.001),
            None => last_duration.max(0.001),
        };
        out.push_str(&format!("file '{name}'\nduration {d:.3}\n"));
    }
    if let Some((name, _)) = frames.last() {
        out.push_str(&format!("file '{name}'\n"));
    }
    out
}

/// Seconds of video `ffconcat` describes for the same inputs.
pub fn total_duration(frames: &[(String, f64)], last_duration: f64) -> f64 {
    match (frames.first(), frames.last()) {
        (Some((_, first)), Some((_, last))) => (last - first).max(0.0) + last_duration.max(0.001),
        _ => 0.0,
    }
}

/// The ffmpeg argument vector (after the binary name). Paths are relative to
/// the recording directory, which is the command's working directory.
pub fn ffmpeg_args(concat_file: &str, output: &str) -> Vec<String> {
    [
        "-nostdin",
        "-loglevel",
        "error",
        "-f",
        "concat",
        "-safe",
        "0",
        "-i",
        concat_file,
        "-vf",
        "fps=30,scale=trunc(iw/2)*2:trunc(ih/2)*2,format=yuv420p",
        "-c:v",
        "libx264",
        "-preset",
        "veryfast",
        "-crf",
        "23",
        "-movflags",
        "+faststart",
        "-y",
        output,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// The last `max` characters of `s`, for reporting an encoder's stderr.
fn tail(s: &str, max: usize) -> String {
    let t = s.trim();
    let n = t.chars().count();
    t.chars().skip(n.saturating_sub(max)).collect()
}

/// Decode standard base64 (padding and ASCII whitespace tolerated).
pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\n' | b'\r' | b' ' | b'\t' => continue,
            _ => return None,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Width and height from a PNG's IHDR chunk.
pub fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    if png.len() < 24 || &png[..8] != b"\x89PNG\r\n\x1a\n" || &png[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(png[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(png[20..24].try_into().ok()?);
    Some((w, h))
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

fn io_fail(what: &str, e: std::io::Error) -> BrowserError {
    BrowserError::Failed(format!("{what}: {e}"))
}

/// Create `media/<parts...>` with each new directory private to the owner.
pub fn create_private_dirs(media: &Path, parts: &[&str]) -> Result<PathBuf, BrowserError> {
    let mut p = media.to_path_buf();
    let mut chain = vec![p.clone()];
    for part in parts {
        p.push(part);
        chain.push(p.clone());
    }
    for dir in chain {
        let mut b = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            b.mode(0o700);
        }
        match b.create(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            // The media root's own parents may not exist yet.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(&dir).map_err(|e| io_fail("create media dir", e))?;
            }
            Err(e) => return Err(io_fail("create media dir", e)),
        }
    }
    Ok(p)
}

/// Write a base64 PNG under `media/screenshots/` and describe it. The
/// dimensions are read from the file when the caller does not know them.
pub fn save_screenshot(
    media: &Path,
    base64: &str,
    width: u32,
    height: u32,
) -> Result<Value, BrowserError> {
    let bytes = b64_decode(base64)
        .ok_or_else(|| BrowserError::Failed("screenshot was not valid base64".into()))?;
    let (w, h) = match (width, height) {
        (0, _) | (_, 0) => png_size(&bytes).unwrap_or((0, 0)),
        wh => wh,
    };
    let dir = create_private_dirs(media, &["screenshots"])?;
    let path = dir.join(format!("{}.png", next_media_name()));
    std::fs::write(&path, &bytes).map_err(|e| io_fail("write screenshot", e))?;
    Ok(json!({
        "path": path.to_string_lossy(),
        "width": w,
        "height": h,
        "bytes": bytes.len(),
    }))
}

/// What a recording's capture task reports back.
#[derive(Default)]
struct Shared {
    /// (file name, seconds since the recording began) per saved frame.
    frames: Vec<(String, f64)>,
    /// Why the task ended; `None` while it runs.
    ended: Option<&'static str>,
    error: Option<String>,
    /// Seconds since the start when the task ended.
    end_elapsed: f64,
}

struct Recording {
    id: String,
    target: String,
    browser_id: u32,
    dir: PathBuf,
    opts: ScreencastOpts,
    started: Instant,
    shared: Arc<Mutex<Shared>>,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

/// The active recordings of one backend, by recording id.
#[derive(Default)]
pub struct ScreencastHub {
    recs: Mutex<HashMap<String, Recording>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Open the capture session: attach, then keep the page rendering even when
/// its window is hidden or unfocused. Focus emulation lives and dies with
/// this connection, which is why the task owns it.
async fn open_session(
    host: &str,
    port: u16,
    target: &str,
    first: bool,
) -> Result<CdpConn, BrowserError> {
    let list = http_json(host, port, "GET", "/json/list").await?;
    let ws = list
        .as_array()
        .into_iter()
        .flatten()
        .find(|t| t.get("id").and_then(Value::as_str) == Some(target))
        .and_then(|t| t.get("webSocketDebuggerUrl").and_then(Value::as_str))
        .ok_or_else(|| BrowserError::NotFound(format!("target '{target}' is gone")))?
        .to_string();
    let mut c = CdpConn::connect(&ws).await?;
    c.call("Page.enable", json!({})).await?;
    if first {
        c.call("Page.bringToFront", json!({})).await.ok();
    }
    c.call(
        "Emulation.setFocusEmulationEnabled",
        json!({ "enabled": true }),
    )
    .await
    .ok();
    Ok(c)
}

async fn grab(c: &mut CdpConn, quality: u32) -> Result<Vec<u8>, BrowserError> {
    let r = c
        .call(
            "Page.captureScreenshot",
            json!({
                "format": "jpeg",
                "quality": quality,
                "optimizeForSpeed": true,
                "fromSurface": true,
            }),
        )
        .await?;
    let data = r
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| BrowserError::Failed("captureScreenshot returned no data".into()))?;
    b64_decode(data).ok_or_else(|| BrowserError::Failed("frame was not valid base64".into()))
}

/// Sleep for `d`, or return true early if a stop was requested.
async fn stopped_within(stop: &mut watch::Receiver<bool>, d: Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(d) => *stop.borrow(),
        _ = stop.changed() => true,
    }
}

struct Capture {
    host: String,
    port: u16,
    target: String,
    dir: PathBuf,
    opts: ScreencastOpts,
    started: Instant,
    shared: Arc<Mutex<Shared>>,
}

async fn capture_loop(cap: Capture, first: CdpConn, mut stop: watch::Receiver<bool>) {
    let period = Duration::from_secs_f64(1.0 / cap.opts.fps as f64);
    let limit = Duration::from_secs(cap.opts.max_seconds as u64);
    let mut conn = Some(first);
    let mut failing_since: Option<Instant> = None;
    let mut next_at = Instant::now();
    let (ended, error) = loop {
        if *stop.borrow() {
            break ("stopped", None);
        }
        if cap.started.elapsed() >= limit || lock(&cap.shared).frames.len() >= cap.opts.max_frames()
        {
            break ("max_seconds", None);
        }
        if conn.is_none() {
            match open_session(&cap.host, cap.port, &cap.target, false).await {
                Ok(c) => conn = Some(c),
                Err(BrowserError::NotFound(_)) => break ("target_closed", None),
                Err(e) => {
                    let since = *failing_since.get_or_insert_with(Instant::now);
                    if since.elapsed() > GIVE_UP {
                        break ("failed", Some(err_text(&e)));
                    }
                    if stopped_within(&mut stop, RETRY_EVERY).await {
                        break ("stopped", None);
                    }
                    continue;
                }
            }
        }
        let Some(c) = conn.as_mut() else { continue };
        match grab(c, cap.opts.quality).await {
            Ok(jpeg) => {
                let name = frame_name(lock(&cap.shared).frames.len() + 1);
                let t = cap.started.elapsed().as_secs_f64();
                if let Err(e) = tokio::fs::write(cap.dir.join("frames").join(&name), &jpeg).await {
                    break ("failed", Some(format!("write frame: {e}")));
                }
                lock(&cap.shared).frames.push((name, t));
                failing_since = None;
            }
            Err(e) => {
                // A navigation or a renderer swap can drop the session; the
                // target itself survives, so reconnect rather than quit.
                conn = None;
                let since = *failing_since.get_or_insert_with(Instant::now);
                if since.elapsed() > GIVE_UP {
                    break ("failed", Some(err_text(&e)));
                }
                if stopped_within(&mut stop, RETRY_EVERY).await {
                    break ("stopped", None);
                }
                continue;
            }
        }
        // Pace to the frame rate; a capture that overran skips the sleep.
        next_at += period;
        let now = Instant::now();
        if next_at <= now {
            next_at = now;
        } else if stopped_within(&mut stop, next_at - now).await {
            break ("stopped", None);
        }
    };
    let mut s = lock(&cap.shared);
    s.ended = Some(ended);
    s.error = error;
    s.end_elapsed = cap.started.elapsed().as_secs_f64();
}

impl ScreencastHub {
    pub async fn start(
        &self,
        browser: (u32, &str, u16),
        target: &str,
        media: &Path,
        opts: ScreencastOpts,
    ) -> Result<Value, BrowserError> {
        self.check_free(target)?;
        let (browser_id, host, port) = browser;
        let first = open_session(host, port, target, true).await?;
        let id = next_media_name();
        let dir = create_private_dirs(media, &["screencasts", &id, "frames"])?
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let shared = Arc::new(Mutex::new(Shared::default()));
        let started = Instant::now();
        let (stop, stop_rx) = watch::channel(false);
        let cap = Capture {
            host: host.to_string(),
            port,
            target: target.to_string(),
            dir: dir.clone(),
            opts,
            started,
            shared: shared.clone(),
        };
        let task = tokio::spawn(capture_loop(cap, first, stop_rx));
        let mut recs = lock(&self.recs);
        // A second start may have slipped in while this one connected.
        let busy = recs.values().any(|r| r.target == target);
        if busy {
            task.abort();
            return Err(already(target));
        }
        recs.insert(
            id.clone(),
            Recording {
                id: id.clone(),
                target: target.to_string(),
                browser_id,
                dir: dir.clone(),
                opts,
                started,
                shared,
                stop,
                task,
            },
        );
        Ok(json!({
            "recording_id": id,
            "dir": dir.to_string_lossy(),
            "fps": opts.fps,
            "quality": opts.quality,
            "max_seconds": opts.max_seconds,
        }))
    }

    fn check_free(&self, target: &str) -> Result<(), BrowserError> {
        if lock(&self.recs).values().any(|r| r.target == target) {
            return Err(already(target));
        }
        Ok(())
    }

    pub fn status(&self) -> Value {
        let recs = lock(&self.recs);
        let mut list: Vec<&Recording> = recs.values().collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        let rows: Vec<Value> = list
            .iter()
            .map(|r| {
                let s = lock(&r.shared);
                json!({
                    "recording_id": r.id,
                    "target_id": r.target,
                    "frames": s.frames.len(),
                    "elapsed_s": (r.started.elapsed().as_secs_f64() * 10.0).round() / 10.0,
                    "running": s.ended.is_none(),
                })
            })
            .collect();
        json!({ "recordings": rows })
    }

    /// Stop every recording of a browser that is going away. The frames
    /// already on disk stay; nothing is encoded.
    pub fn abort_browser(&self, browser_id: u32) {
        lock(&self.recs).retain(|_, r| {
            if r.browser_id == browser_id {
                r.task.abort();
                false
            } else {
                true
            }
        });
    }

    pub fn abort_all(&self) {
        for (_, r) in lock(&self.recs).drain() {
            r.task.abort();
        }
    }

    pub async fn stop(
        &self,
        target: Option<&str>,
        recording_id: Option<&str>,
        keep_frames: bool,
    ) -> Result<Value, BrowserError> {
        let mut rec = {
            let mut recs = lock(&self.recs);
            let key = recs
                .values()
                .find(|r| {
                    recording_id.is_some_and(|i| i == r.id) || target.is_some_and(|t| t == r.target)
                })
                .map(|r| r.id.clone());
            key.and_then(|k| recs.remove(&k))
        }
        .ok_or_else(|| {
            BrowserError::NotFound(
                "no recording matches (browser_screencast status lists the active ones)".into(),
            )
        })?;
        rec.stop.send(true).ok();
        if tokio::time::timeout(STOP_GRACE, &mut rec.task)
            .await
            .is_err()
        {
            rec.task.abort();
        }
        let (frames, ended, error, end_elapsed) = {
            let s = lock(&rec.shared);
            (
                s.frames.clone(),
                s.ended.unwrap_or("stopped"),
                s.error.clone(),
                if s.ended.is_some() {
                    s.end_elapsed
                } else {
                    rec.started.elapsed().as_secs_f64()
                },
            )
        };
        let last = frames.last().map_or(0.0, |(_, t)| *t);
        let last_duration = (end_elapsed - last).clamp(1.0 / rec.opts.fps as f64, 1.0);
        let duration = total_duration(&frames, last_duration);
        let concat = rec.dir.join("frames.ffconcat");
        let listed: Vec<(String, f64)> = frames
            .iter()
            .map(|(n, t)| (format!("frames/{n}"), *t))
            .collect();
        tokio::fs::write(&concat, ffconcat(&listed, last_duration))
            .await
            .map_err(|e| io_fail("write frames.ffconcat", e))?;

        let (ffmpeg, mp4) = if frames.is_empty() {
            ("failed: no frames were captured".to_string(), None)
        } else {
            encode(&rec.dir).await
        };
        let mut note = None;
        if ffmpeg == "missing" {
            note = Some(format!(
                "ffmpeg is not on PATH, so no mp4 was made; frames and frames.ffconcat were kept. \
                 To encode: cd '{}' && ffmpeg {}",
                rec.dir.display(),
                ffmpeg_args("frames.ffconcat", "screencast.mp4").join(" "),
            ));
        } else if mp4.is_some() && !keep_frames {
            let _ = tokio::fs::remove_dir_all(rec.dir.join("frames")).await;
            let _ = tokio::fs::remove_file(&concat).await;
        }
        let mut out = json!({
            "path": mp4.map(|p| p.to_string_lossy().into_owned()),
            "frames": frames.len(),
            "duration_s": (duration * 1000.0).round() / 1000.0,
            "effective_fps": if duration > 0.0 {
                ((frames.len() as f64 / duration) * 10.0).round() / 10.0
            } else {
                0.0
            },
            "dir": rec.dir.to_string_lossy(),
            "ended": ended,
            "ffmpeg": ffmpeg,
        });
        if let Some(e) = error {
            out["error"] = json!(e);
        }
        if let Some(n) = note {
            out["note"] = json!(n);
        }
        Ok(out)
    }
}

fn already(target: &str) -> BrowserError {
    BrowserError::Failed(format!(
        "target '{target}' already has a recording (browser_screencast stop it first)"
    ))
}

/// Run ffmpeg over `dir/frames.ffconcat`. Returns the status string and the
/// mp4 path on success.
async fn encode(dir: &Path) -> (String, Option<PathBuf>) {
    let mut cmd = tokio::process::Command::new("ffmpeg");
    cmd.args(ffmpeg_args("frames.ffconcat", "screencast.mp4"))
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ("missing".into(), None),
        Err(e) => return (format!("failed: {e}"), None),
    };
    match tokio::time::timeout(FFMPEG_TIMEOUT, child.wait_with_output()).await {
        Err(_) => ("failed: ffmpeg timed out".into(), None),
        Ok(Err(e)) => (format!("failed: {e}"), None),
        Ok(Ok(o)) if o.status.success() => ("ok".into(), Some(dir.join("screencast.mp4"))),
        Ok(Ok(o)) => (
            format!("failed: {}", tail(&String::from_utf8_lossy(&o.stderr), 600)),
            None,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opts_default_and_clamp() {
        let d = ScreencastOpts::from_args(None, None, None);
        assert_eq!((d.fps, d.quality, d.max_seconds), (15, 80, 300));
        let lo = ScreencastOpts::from_args(Some(0), Some(-5), Some(0));
        assert_eq!((lo.fps, lo.quality, lo.max_seconds), (1, 30, 1));
        let hi = ScreencastOpts::from_args(Some(500), Some(100), Some(99_999));
        assert_eq!((hi.fps, hi.quality, hi.max_seconds), (30, 95, 1800));
        assert_eq!(hi.max_frames(), 30 * 1800);
    }

    #[test]
    fn names_are_fixed_width_and_distinct() {
        assert_eq!(media_name(1_760_000_000_123, 255), "1760000000123-00ff");
        assert_eq!(frame_name(7), "00007.jpg");
        assert_ne!(next_media_name(), next_media_name());
        assert!(!next_media_name().contains('/'));
    }

    #[test]
    fn ffconcat_uses_gaps_and_repeats_last_file() {
        let frames = vec![
            ("00001.jpg".to_string(), 0.10),
            ("00002.jpg".to_string(), 0.30),
            ("00003.jpg".to_string(), 0.35),
        ];
        let t = ffconcat(&frames, 0.2);
        assert_eq!(
            t,
            "ffconcat version 1.0\n\
             file '00001.jpg'\nduration 0.200\n\
             file '00002.jpg'\nduration 0.050\n\
             file '00003.jpg'\nduration 0.200\n\
             file '00003.jpg'\n"
        );
        assert!((total_duration(&frames, 0.2) - 0.45).abs() < 1e-9);
        assert_eq!(ffconcat(&[], 0.2), "ffconcat version 1.0\n");
        assert_eq!(total_duration(&[], 0.2), 0.0);
    }

    #[test]
    fn ffmpeg_args_shape() {
        let a = ffmpeg_args("frames.ffconcat", "screencast.mp4");
        let joined = a.join(" ");
        assert!(joined.contains("-f concat -safe 0 -i frames.ffconcat"));
        assert!(joined.contains("fps=30,scale=trunc(iw/2)*2:trunc(ih/2)*2,format=yuv420p"));
        assert!(joined.contains("-c:v libx264 -preset veryfast -crf 23 -movflags +faststart -y"));
        assert_eq!(a.last().map(String::as_str), Some("screencast.mp4"));
    }

    #[test]
    fn base64_and_png_header() {
        assert_eq!(b64_decode("aGVsbG8=").as_deref(), Some(&b"hello"[..]));
        assert_eq!(b64_decode("aGVs\nbG8").as_deref(), Some(&b"hello"[..]));
        assert_eq!(b64_decode("a$b"), None);
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&640u32.to_be_bytes());
        png.extend_from_slice(&480u32.to_be_bytes());
        assert_eq!(png_size(&png), Some((640, 480)));
        assert_eq!(png_size(b"not a png at all, no"), None);
    }

    #[test]
    fn tail_keeps_the_end() {
        assert_eq!(tail("  abcdef \n", 3), "def");
        assert_eq!(tail("ab", 10), "ab");
    }

    #[test]
    fn save_screenshot_writes_under_media_only() {
        let media = std::env::temp_dir().join(format!("agentctl-shot-{}", next_media_name()));
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&3u32.to_be_bytes());
        png.extend_from_slice(&2u32.to_be_bytes());
        // base64 of the 24 bytes above
        let b64 = "iVBORw0KGgoAAAANSUhEUgAAAAMAAAAC";
        let v = save_screenshot(&media, b64, 0, 0).unwrap();
        let path = PathBuf::from(v["path"].as_str().unwrap());
        assert!(path.starts_with(media.join("screenshots")));
        assert_eq!(std::fs::read(&path).unwrap(), png);
        assert_eq!(
            (v["width"].as_u64(), v["height"].as_u64()),
            (Some(3), Some(2))
        );
        assert_eq!(v["bytes"].as_u64(), Some(24));
        std::fs::remove_dir_all(&media).ok();
    }
}
