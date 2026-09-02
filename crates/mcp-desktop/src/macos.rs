//! Real macOS backend for the desktop engine.
//!
//! Every call goes through a platform CLI. Where macOS genuinely exposes no
//! interface — screen brightness has no supported read/write path, and Focus
//! modes replaced the scriptable Do-Not-Disturb — the call returns
//! `Unsupported` naming the reason rather than pretending.

use std::path::Path;
use std::process::Command;

use async_trait::async_trait;
use mcp_policy::applescript_escape;
use serde_json::{json, Value};

use crate::backend::{DesktopBackend, DesktopError, IdleStatus, MediaAction, PowerAction};

pub struct MacosDesktop;

impl Default for MacosDesktop {
    fn default() -> Self {
        Self::new()
    }
}

impl MacosDesktop {
    pub fn new() -> Self {
        MacosDesktop
    }
}

fn run(program: &str, args: &[&str]) -> Result<String, DesktopError> {
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| DesktopError::Failed(format!("{program}: {e}")))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(DesktopError::Failed(if err.is_empty() {
            format!("{program} exited {}", out.status)
        } else {
            err
        }));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Run an AppleScript. Callers must escape any interpolated text with
/// [`applescript_escape`] first.
fn osa(script: &str) -> Result<String, DesktopError> {
    run("/usr/bin/osascript", &["-e", script]).map(|s| s.trim().to_string())
}

/// Is this app running? Asking first keeps AppleScript from *launching* a media
/// player as a side effect of a transport command.
fn app_running(name: &str) -> bool {
    Command::new("/usr/bin/pgrep")
        .args(["-x", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Seconds since the last HID event, from the IOKit registry.
fn hid_idle_seconds() -> Result<u64, DesktopError> {
    let text = run("/usr/sbin/ioreg", &["-c", "IOHIDSystem", "-d", "4"])?;
    let ns = text
        .lines()
        .find(|l| l.contains("HIDIdleTime"))
        .and_then(|l| l.rsplit('=').next())
        .map(|v| v.trim().trim_matches(|c: char| !c.is_ascii_digit()))
        .and_then(|v| v.parse::<u128>().ok())
        .ok_or_else(|| DesktopError::Failed("HIDIdleTime not present in ioreg output".into()))?;
    Ok((ns / 1_000_000_000) as u64)
}

fn screen_locked() -> bool {
    run(
        "/usr/sbin/ioreg",
        &["-n", "Root", "-d", "1", "-k", "CGSSessionScreenIsLocked"],
    )
    .map(|t| t.contains("CGSSessionScreenIsLocked\" = Yes"))
    .unwrap_or(false)
}

#[async_trait]
impl DesktopBackend for MacosDesktop {
    async fn lock_screen(&self) -> Result<(), DesktopError> {
        // The session-suspend entry point behind the Apple menu's "Lock Screen".
        run(
            "/System/Library/CoreServices/Menu Extras/User.menu/Contents/Resources/CGSession",
            &["-suspend"],
        )
        .map(|_| ())
    }

    async fn notify(
        &self,
        title: &str,
        body: &str,
        subtitle: Option<&str>,
        sound: bool,
    ) -> Result<(), DesktopError> {
        let mut script = format!(
            "display notification \"{}\" with title \"{}\"",
            applescript_escape(body),
            applescript_escape(title)
        );
        if let Some(s) = subtitle {
            script.push_str(&format!(" subtitle \"{}\"", applescript_escape(s)));
        }
        if sound {
            script.push_str(" sound name \"Ping\"");
        }
        osa(&script).map(|_| ())
    }

    async fn idle_status(&self) -> Result<IdleStatus, DesktopError> {
        let idle_seconds = hid_idle_seconds()?;
        let locked = screen_locked();
        Ok(IdleStatus {
            idle_seconds,
            locked,
            // "Present" is a judgement, not a sensor reading: recent input and an
            // unlocked screen is the best signal the OS actually offers.
            user_present: !locked && idle_seconds < 300,
        })
    }

    async fn setting_get(&self, setting: &str) -> Result<Value, DesktopError> {
        match setting {
            "volume" => {
                let v = osa("output volume of (get volume settings)")?;
                let muted = osa("output muted of (get volume settings)")? == "true";
                let level = v
                    .parse::<i64>()
                    .map_err(|_| DesktopError::Failed(format!("unparsable volume '{v}'")))?;
                Ok(json!({ "volume": level, "muted": muted }))
            }
            "dark_mode" => {
                let v = osa(
                    "tell application \"System Events\" to tell appearance preferences to get dark mode",
                )?;
                Ok(json!({ "dark_mode": v == "true" }))
            }
            "resolution" => {
                let text = run("/usr/sbin/system_profiler", &["SPDisplaysDataType"])?;
                let modes: Vec<String> = text
                    .lines()
                    .filter(|l| l.trim_start().starts_with("Resolution:"))
                    .map(|l| {
                        l.trim()
                            .trim_start_matches("Resolution:")
                            .trim()
                            .to_string()
                    })
                    .collect();
                Ok(json!({ "displays": modes }))
            }
            "brightness" => Err(DesktopError::Unsupported(
                "macOS exposes no supported read path for display brightness".into(),
            )),
            "dnd" => Err(DesktopError::Unsupported(
                "Do Not Disturb was replaced by Focus modes, which are not scriptable".into(),
            )),
            other => Err(DesktopError::NotFound(format!("unknown setting '{other}'"))),
        }
    }

    async fn setting_set(&self, setting: &str, value: &str) -> Result<(), DesktopError> {
        match setting {
            "volume" => {
                let level: i64 = value.parse().map_err(|_| {
                    DesktopError::Failed(format!("volume must be 0-100, got '{value}'"))
                })?;
                if !(0..=100).contains(&level) {
                    return Err(DesktopError::Failed(format!(
                        "volume must be 0-100, got {level}"
                    )));
                }
                osa(&format!("set volume output volume {level}")).map(|_| ())
            }
            "dark_mode" => {
                let on = matches!(value, "true" | "on" | "1");
                osa(&format!(
                    "tell application \"System Events\" to tell appearance preferences to set dark mode to {on}"
                ))
                .map(|_| ())
            }
            "brightness" => Err(DesktopError::Unsupported(
                "macOS exposes no supported write path for display brightness".into(),
            )),
            "resolution" => Err(DesktopError::Unsupported(
                "changing display mode needs a private CoreDisplay API; not exposed".into(),
            )),
            "dnd" => Err(DesktopError::Unsupported(
                "Focus modes replaced Do Not Disturb and are not scriptable".into(),
            )),
            other => Err(DesktopError::NotFound(format!("unknown setting '{other}'"))),
        }
    }

    async fn media(&self, action: MediaAction) -> Result<String, DesktopError> {
        // Media keys are `NSSystemDefined` events, which CGEvent cannot post
        // without an Objective-C bridge. Driving the running player directly is
        // both simpler and more predictable — and it never launches one.
        const PLAYERS: &[&str] = &["Spotify", "Music", "TV", "VLC", "QuickTime Player"];
        let Some(app) = PLAYERS.iter().find(|p| app_running(p)) else {
            return Err(DesktopError::NotFound(
                "no supported media player is running (Spotify, Music, TV, VLC, QuickTime)".into(),
            ));
        };
        let verb = match (action, *app) {
            (MediaAction::PlayPause, _) => "playpause",
            (MediaAction::Next, "VLC") => "next",
            (MediaAction::Next, _) => "next track",
            (MediaAction::Prev, "VLC") => "previous",
            (MediaAction::Prev, _) => "previous track",
            (MediaAction::Stop, _) => "stop",
        };
        osa(&format!(
            "tell application \"{}\" to {verb}",
            applescript_escape(app)
        ))?;
        Ok((*app).to_string())
    }

    async fn power(&self, action: PowerAction, delay_s: u64) -> Result<(), DesktopError> {
        if delay_s > 0 {
            std::thread::sleep(std::time::Duration::from_secs(delay_s.min(300)));
        }
        match action {
            PowerAction::Sleep => run("/usr/bin/pmset", &["sleepnow"]).map(|_| ()),
            PowerAction::Logout => osa("tell application \"System Events\" to log out").map(|_| ()),
            PowerAction::Restart => {
                osa("tell application \"System Events\" to restart").map(|_| ())
            }
            PowerAction::Shutdown => {
                osa("tell application \"System Events\" to shut down").map(|_| ())
            }
        }
    }

    async fn speak(
        &self,
        text: &str,
        voice: Option<&str>,
        rate: Option<u32>,
    ) -> Result<(), DesktopError> {
        // argv form: the text is data, never part of a shell string.
        let mut args: Vec<String> = Vec::new();
        if let Some(v) = voice {
            args.push("-v".into());
            args.push(v.to_string());
        }
        if let Some(r) = rate {
            args.push("-r".into());
            args.push(r.clamp(50, 500).to_string());
        }
        args.push(text.to_string());
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        run("/usr/bin/say", &refs).map(|_| ())
    }

    async fn play_audio(&self, path: &Path) -> Result<(), DesktopError> {
        let p = path.to_string_lossy().to_string();
        run("/usr/bin/afplay", &[p.as_str()]).map(|_| ())
    }

    fn platform(&self) -> &'static str {
        "macos"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Idle time is read out of the IOKit registry; a live desktop must report
    /// a plausible number rather than an error.
    #[tokio::test]
    async fn idle_status_reads_the_real_session() {
        let d = MacosDesktop::new();
        let s = d
            .idle_status()
            .await
            .expect("live session must be readable");
        assert!(
            s.idle_seconds < 60 * 60 * 24 * 7,
            "implausible idle time {}",
            s.idle_seconds
        );
        assert_eq!(s.user_present, !s.locked && s.idle_seconds < 300);
    }

    #[tokio::test]
    async fn volume_and_dark_mode_are_readable() {
        let d = MacosDesktop::new();
        let v = d.setting_get("volume").await.expect("volume");
        let level = v["volume"].as_i64().expect("numeric volume");
        assert!((0..=100).contains(&level), "volume out of range: {level}");
        assert!(d.setting_get("dark_mode").await.unwrap()["dark_mode"].is_boolean());
    }

    /// Settings macOS does not expose must say so, and say *why*, rather than
    /// failing in a way that reads like a transient error worth retrying.
    #[tokio::test]
    async fn unavailable_settings_explain_themselves() {
        let d = MacosDesktop::new();
        for s in ["brightness", "dnd"] {
            let e = d.setting_get(s).await.unwrap_err();
            match e {
                DesktopError::Unsupported(m) => {
                    assert!(m.len() > 20, "explanation too thin for {s}: {m}")
                }
                other => panic!("{s} should be Unsupported, got {other:?}"),
            }
        }
        assert!(matches!(
            d.setting_get("nonsense").await.unwrap_err(),
            DesktopError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn volume_set_rejects_out_of_range() {
        let d = MacosDesktop::new();
        assert!(d.setting_set("volume", "150").await.is_err());
        assert!(d.setting_set("volume", "abc").await.is_err());
    }
}
