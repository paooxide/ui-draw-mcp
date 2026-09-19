//! Session, power, settings, media and speech over D-Bus and a few
//! well-known commands.
//!
//! GNOME publishes what this needs: idle time from Mutter, the lock screen
//! from `org.gnome.ScreenSaver`, brightness from the settings daemon, and
//! session end from the session manager. Power transitions go through
//! `logind`, which any systemd desktop has. Media control is MPRIS, which
//! every player speaks. Sound is PipeWire's `wpctl`, and speech is
//! `espeak-ng`, the one synthesiser a distribution is likely to ship.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use mcp_desktop::{DesktopBackend, DesktopError, IdleStatus, MediaAction, PowerAction};
use serde_json::{json, Value};
use tokio::sync::OnceCell;
use zbus::zvariant::OwnedValue;

fn fail(e: impl std::fmt::Display) -> DesktopError {
    DesktopError::Failed(e.to_string())
}

/// Run a command with argv (never a shell) and return trimmed stdout.
async fn run(program: &str, args: &[&str]) -> Result<String, DesktopError> {
    let out = tokio::process::Command::new(program)
        .args(args)
        .output()
        .await
        .map_err(|e| DesktopError::Failed(format!("{program}: {e}")))?;
    if !out.status.success() {
        return Err(DesktopError::Failed(format!(
            "{program} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `wpctl get-volume` prints `Volume: 0.45` or `Volume: 0.45 [MUTED]`.
pub fn parse_wpctl_volume(text: &str) -> Result<(i64, bool), String> {
    let t = text.trim();
    let rest = t
        .strip_prefix("Volume:")
        .ok_or_else(|| format!("unexpected wpctl output '{t}'"))?
        .trim();
    let mut parts = rest.split_whitespace();
    let level: f64 = parts
        .next()
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| format!("unparsable volume in '{t}'"))?;
    let muted = parts.any(|p| p.eq_ignore_ascii_case("[muted]"));
    Ok(((level * 100.0).round().clamp(0.0, 150.0) as i64, muted))
}

/// Which MPRIS player to drive: a playing one, else the first.
pub fn choose_player(players: &[(String, String)]) -> Option<&(String, String)> {
    players
        .iter()
        .find(|(_, status)| status == "Playing")
        .or_else(|| players.first())
}

pub struct LinuxDesktop {
    session: OnceCell<zbus::Connection>,
    system: OnceCell<zbus::Connection>,
}

impl Default for LinuxDesktop {
    fn default() -> Self {
        Self::new()
    }
}

impl LinuxDesktop {
    pub fn new() -> Self {
        LinuxDesktop {
            session: OnceCell::new(),
            system: OnceCell::new(),
        }
    }

    async fn session(&self) -> Result<&zbus::Connection, DesktopError> {
        self.session
            .get_or_try_init(zbus::Connection::session)
            .await
            .map_err(|e| DesktopError::Failed(format!("session bus: {e}")))
    }

    async fn system(&self) -> Result<&zbus::Connection, DesktopError> {
        self.system
            .get_or_try_init(zbus::Connection::system)
            .await
            .map_err(|e| DesktopError::Failed(format!("system bus: {e}")))
    }

    async fn proxy<'a>(
        &self,
        conn: &'a zbus::Connection,
        dest: &'static str,
        path: &'static str,
        iface: &'static str,
    ) -> Result<zbus::Proxy<'a>, DesktopError> {
        zbus::Proxy::new(conn, dest, path, iface)
            .await
            .map_err(|e| DesktopError::Failed(format!("{dest}: {e}")))
    }

    async fn gsettings_get(&self, schema: &str, key: &str) -> Result<String, DesktopError> {
        run("/usr/bin/gsettings", &["get", schema, key]).await
    }

    async fn gsettings_set(
        &self,
        schema: &str,
        key: &str,
        value: &str,
    ) -> Result<(), DesktopError> {
        run("/usr/bin/gsettings", &["set", schema, key, value])
            .await
            .map(|_| ())
    }

    /// MPRIS players on the bus as `(bus name, playback status)`.
    async fn players(&self) -> Result<Vec<(String, String)>, DesktopError> {
        let conn = self.session().await?;
        let dbus = zbus::fdo::DBusProxy::new(conn).await.map_err(fail)?;
        let names = dbus.list_names().await.map_err(fail)?;
        let mut out = Vec::new();
        for n in names {
            let name = n.to_string();
            if !name.starts_with("org.mpris.MediaPlayer2.") {
                continue;
            }
            let status = zbus::Proxy::new(
                conn,
                name.clone(),
                "/org/mpris/MediaPlayer2",
                "org.mpris.MediaPlayer2.Player",
            )
            .await
            .ok();
            let status = match status {
                Some(p) => p
                    .get_property::<String>("PlaybackStatus")
                    .await
                    .unwrap_or_default(),
                None => String::new(),
            };
            out.push((name, status));
        }
        Ok(out)
    }
}

#[async_trait]
impl DesktopBackend for LinuxDesktop {
    async fn lock_screen(&self) -> Result<(), DesktopError> {
        let conn = self.session().await?;
        if let Ok(p) = self
            .proxy(
                conn,
                "org.gnome.ScreenSaver",
                "/org/gnome/ScreenSaver",
                "org.gnome.ScreenSaver",
            )
            .await
        {
            if p.call::<_, _, ()>("Lock", &()).await.is_ok() {
                return Ok(());
            }
        }
        // Any logind desktop.
        run("/usr/bin/loginctl", &["lock-session"])
            .await
            .map(|_| ())
    }

    async fn notify(
        &self,
        title: &str,
        body: &str,
        subtitle: Option<&str>,
        sound: bool,
    ) -> Result<(), DesktopError> {
        let mut text = String::new();
        if let Some(s) = subtitle {
            text.push_str(s);
            text.push('\n');
        }
        text.push_str(body);
        let mut n = notify_rust::Notification::new();
        n.appname("agentctl")
            .summary(title)
            .body(&text)
            .timeout(notify_rust::Timeout::Milliseconds(15_000));
        if sound {
            n.sound_name("message-new-instant");
        }
        let n = n.finalize();
        tokio::task::spawn_blocking(move || n.show().map(|_| ()))
            .await
            .map_err(fail)?
            .map_err(|e| DesktopError::Failed(format!("notification: {e}")))
    }

    async fn idle_status(&self) -> Result<IdleStatus, DesktopError> {
        let conn = self.session().await?;
        let idle = self
            .proxy(
                conn,
                "org.gnome.Mutter.IdleMonitor",
                "/org/gnome/Mutter/IdleMonitor/Core",
                "org.gnome.Mutter.IdleMonitor",
            )
            .await?;
        let ms: u64 = idle.call("GetIdletime", &()).await.map_err(|e| {
            DesktopError::Unsupported(format!(
                "idle time needs Mutter's IdleMonitor, which this session does not offer: {e}"
            ))
        })?;
        let locked = match self
            .proxy(
                conn,
                "org.gnome.ScreenSaver",
                "/org/gnome/ScreenSaver",
                "org.gnome.ScreenSaver",
            )
            .await
        {
            Ok(p) => p
                .call::<_, _, bool>("GetActive", &())
                .await
                .unwrap_or(false),
            Err(_) => false,
        };
        let idle_seconds = ms / 1000;
        Ok(IdleStatus {
            idle_seconds,
            locked,
            user_present: !locked && idle_seconds < 300,
        })
    }

    async fn setting_get(&self, setting: &str) -> Result<Value, DesktopError> {
        match setting {
            "volume" => {
                let text = run("/usr/bin/wpctl", &["get-volume", "@DEFAULT_AUDIO_SINK@"]).await?;
                let (volume, muted) = parse_wpctl_volume(&text).map_err(DesktopError::Failed)?;
                Ok(json!({ "volume": volume, "muted": muted }))
            }
            "dark_mode" => {
                let v = self
                    .gsettings_get("org.gnome.desktop.interface", "color-scheme")
                    .await?;
                Ok(json!({ "dark_mode": v.contains("prefer-dark") }))
            }
            "brightness" => {
                let conn = self.session().await?;
                let p = self
                    .proxy(
                        conn,
                        "org.gnome.SettingsDaemon.Power",
                        "/org/gnome/SettingsDaemon/Power",
                        "org.gnome.SettingsDaemon.Power.Screen",
                    )
                    .await?;
                let b: i32 = p.get_property("Brightness").await.map_err(|e| {
                    DesktopError::Unsupported(format!(
                        "no brightness interface on this session: {e}"
                    ))
                })?;
                if b < 0 {
                    return Err(DesktopError::Unsupported(
                        "this display has no software-controllable backlight".into(),
                    ));
                }
                Ok(json!({ "brightness": b }))
            }
            "resolution" => {
                let ms = crate::vision::monitors_public()
                    .await
                    .map_err(DesktopError::Unsupported)?;
                let displays: Vec<String> = ms
                    .iter()
                    .map(|m| {
                        format!(
                            "{} {}x{} @{}x at ({}, {})",
                            m.connector, m.w, m.h, m.scale, m.x, m.y
                        )
                    })
                    .collect();
                Ok(json!({ "displays": displays }))
            }
            "dnd" => {
                let v = self
                    .gsettings_get("org.gnome.desktop.notifications", "show-banners")
                    .await?;
                Ok(json!({ "dnd": v.trim() == "false" }))
            }
            other => Err(DesktopError::NotFound(format!("unknown setting '{other}'"))),
        }
    }

    async fn setting_set(&self, setting: &str, value: &str) -> Result<(), DesktopError> {
        match setting {
            "volume" => {
                let level: i64 = value
                    .parse()
                    .map_err(|_| DesktopError::Failed(format!("volume must be 0-100, got '{value}'")))?;
                if !(0..=100).contains(&level) {
                    return Err(DesktopError::Failed(format!("volume must be 0-100, got {level}")));
                }
                run("/usr/bin/wpctl", &["set-volume", "@DEFAULT_AUDIO_SINK@", &format!("{level}%")])
                    .await
                    .map(|_| ())
            }
            "dark_mode" => {
                let on = matches!(value, "true" | "on" | "1");
                self.gsettings_set(
                    "org.gnome.desktop.interface",
                    "color-scheme",
                    if on { "prefer-dark" } else { "default" },
                )
                .await
            }
            "brightness" => {
                let level: i32 = value
                    .parse()
                    .map_err(|_| DesktopError::Failed(format!("brightness must be 0-100, got '{value}'")))?;
                if !(0..=100).contains(&level) {
                    return Err(DesktopError::Failed(format!("brightness must be 0-100, got {level}")));
                }
                let conn = self.session().await?;
                let p = self
                    .proxy(
                        conn,
                        "org.gnome.SettingsDaemon.Power",
                        "/org/gnome/SettingsDaemon/Power",
                        "org.gnome.SettingsDaemon.Power.Screen",
                    )
                    .await?;
                let current: i32 = p.get_property("Brightness").await.map_err(|e| {
                    DesktopError::Unsupported(format!("no brightness interface on this session: {e}"))
                })?;
                if current < 0 {
                    return Err(DesktopError::Unsupported(
                        "this display has no software-controllable backlight".into(),
                    ));
                }
                p.set_property("Brightness", level)
                    .await
                    .map_err(|e| DesktopError::Failed(format!("brightness: {e}")))
            }
            "dnd" => {
                let on = matches!(value, "true" | "on" | "1");
                self.gsettings_set(
                    "org.gnome.desktop.notifications",
                    "show-banners",
                    if on { "false" } else { "true" },
                )
                .await
            }
            "resolution" => Err(DesktopError::Unsupported(
                "changing display modes goes through Mutter's ApplyMonitorsConfig, which is not exposed here".into(),
            )),
            other => Err(DesktopError::NotFound(format!("unknown setting '{other}'"))),
        }
    }

    async fn media(&self, action: MediaAction) -> Result<String, DesktopError> {
        let players = self.players().await?;
        let Some((name, _)) = choose_player(&players) else {
            return Err(DesktopError::NotFound(
                "no MPRIS media player is running".into(),
            ));
        };
        let conn = self.session().await?;
        let p = zbus::Proxy::new(
            conn,
            name.clone(),
            "/org/mpris/MediaPlayer2",
            "org.mpris.MediaPlayer2.Player",
        )
        .await
        .map_err(fail)?;
        let method = match action {
            MediaAction::PlayPause => "PlayPause",
            MediaAction::Next => "Next",
            MediaAction::Prev => "Previous",
            MediaAction::Stop => "Stop",
        };
        p.call::<_, _, ()>(method, &())
            .await
            .map_err(|e| DesktopError::Failed(format!("{name} {method}: {e}")))?;
        let identity = zbus::Proxy::new(
            conn,
            name.clone(),
            "/org/mpris/MediaPlayer2",
            "org.mpris.MediaPlayer2",
        )
        .await
        .ok();
        let identity = match identity {
            Some(ip) => ip.get_property::<String>("Identity").await.ok(),
            None => None,
        };
        Ok(identity.unwrap_or_else(|| {
            name.trim_start_matches("org.mpris.MediaPlayer2.")
                .to_string()
        }))
    }

    async fn power(&self, action: PowerAction, delay_s: u64) -> Result<(), DesktopError> {
        if delay_s > 0 {
            tokio::time::sleep(Duration::from_secs(delay_s.min(300))).await;
        }
        match action {
            PowerAction::Logout => {
                let conn = self.session().await?;
                let p = self
                    .proxy(
                        conn,
                        "org.gnome.SessionManager",
                        "/org/gnome/SessionManager",
                        "org.gnome.SessionManager",
                    )
                    .await?;
                // Mode 1: no confirmation dialog. The agent asked for a logout.
                p.call::<_, _, ()>("Logout", &(1u32,))
                    .await
                    .map_err(|e| DesktopError::Failed(format!("logout: {e}")))
            }
            other => {
                let conn = self.system().await?;
                let p = self
                    .proxy(
                        conn,
                        "org.freedesktop.login1",
                        "/org/freedesktop/login1",
                        "org.freedesktop.login1.Manager",
                    )
                    .await?;
                let method = match other {
                    PowerAction::Sleep => "Suspend",
                    PowerAction::Restart => "Reboot",
                    PowerAction::Shutdown => "PowerOff",
                    PowerAction::Logout => unreachable!(),
                };
                p.call::<_, _, ()>(method, &(true,))
                    .await
                    .map_err(|e| DesktopError::Failed(format!("logind {method}: {e}")))
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
            args.push("-s".into());
            args.push(r.clamp(80, 450).to_string());
        }
        args.push("--".into());
        args.push(text.to_string());
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        match run("/usr/bin/espeak-ng", &refs).await {
            Ok(_) => Ok(()),
            Err(DesktopError::Failed(m)) if m.contains("No such file") => Err(
                DesktopError::Unsupported("speech needs espeak-ng (dnf install espeak-ng)".into()),
            ),
            Err(e) => Err(e),
        }
    }

    async fn play_audio(&self, path: &Path) -> Result<(), DesktopError> {
        let p = path.to_string_lossy().to_string();
        for (bin, args) in [
            ("/usr/bin/pw-play", vec![p.as_str()]),
            ("/usr/bin/paplay", vec![p.as_str()]),
            ("/usr/bin/aplay", vec!["-q", p.as_str()]),
        ] {
            if !Path::new(bin).exists() {
                continue;
            }
            return run(bin, &args).await.map(|_| ());
        }
        Err(DesktopError::Unsupported(
            "no audio player found (pw-play, paplay or aplay)".into(),
        ))
    }

    fn platform(&self) -> &'static str {
        "linux"
    }
}

#[allow(dead_code)]
fn _owned_value_used(_: OwnedValue, _: HashMap<String, OwnedValue>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wpctl_output_parses_level_and_mute() {
        assert_eq!(parse_wpctl_volume("Volume: 0.45").unwrap(), (45, false));
        assert_eq!(
            parse_wpctl_volume("Volume: 0.10 [MUTED]\n").unwrap(),
            (10, true)
        );
        assert_eq!(parse_wpctl_volume("Volume: 1.00").unwrap(), (100, false));
        assert_eq!(parse_wpctl_volume("Volume: 0.00").unwrap(), (0, false));
        // Boosted sinks report above 1.0; clamp rather than reject.
        assert_eq!(parse_wpctl_volume("Volume: 1.53").unwrap(), (150, false));
        assert!(parse_wpctl_volume("").is_err());
        assert!(parse_wpctl_volume("Volume: loud").is_err());
        assert!(parse_wpctl_volume("Mute: yes").is_err());
    }

    #[test]
    fn a_playing_player_is_preferred_over_the_first() {
        let players = vec![
            ("org.mpris.MediaPlayer2.a".to_string(), "Paused".to_string()),
            (
                "org.mpris.MediaPlayer2.b".to_string(),
                "Playing".to_string(),
            ),
        ];
        assert_eq!(
            choose_player(&players).unwrap().0,
            "org.mpris.MediaPlayer2.b"
        );
        let players = vec![(
            "org.mpris.MediaPlayer2.a".to_string(),
            "Stopped".to_string(),
        )];
        assert_eq!(
            choose_player(&players).unwrap().0,
            "org.mpris.MediaPlayer2.a"
        );
        assert!(choose_player(&[]).is_none());
    }
}
