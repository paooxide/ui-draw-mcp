//! What `agentctl doctor` reports about this Linux session.
//!
//! Every desktop engine here depends on something that can be missing
//! without an error anywhere: no accessibility bus and every tree is empty,
//! no portal and every keystroke is refused, no zenity and every consent is
//! a denial. The report names each one and what to do about it.

use std::path::Path;

/// One line of the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
    /// What to do when it is not ok. Empty when nothing is needed.
    pub fix: &'static str,
}

#[derive(Debug, Clone, Default)]
pub struct Doctor {
    pub session_type: String,
    pub desktop: String,
    pub checks: Vec<Check>,
}

impl Doctor {
    pub fn lines(&self) -> Vec<String> {
        let mut out = vec![format!(
            "  session:         {} ({})",
            if self.session_type.is_empty() {
                "unknown"
            } else {
                &self.session_type
            },
            if self.desktop.is_empty() {
                "no XDG_CURRENT_DESKTOP"
            } else {
                &self.desktop
            }
        )];
        for c in &self.checks {
            out.push(format!(
                "  {:<17}{} {}",
                format!("{}:", c.name),
                if c.ok { "ok" } else { "MISSING" },
                c.detail
            ));
            if !c.ok && !c.fix.is_empty() {
                out.push(format!("    -> {}", c.fix));
            }
        }
        out
    }
}

async fn portal_version(iface: &str) -> Option<u32> {
    let conn = zbus::Connection::session().await.ok()?;
    let p = zbus::Proxy::new(
        &conn,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        format!("org.freedesktop.portal.{iface}"),
    )
    .await
    .ok()?;
    p.get_property::<u32>("version").await.ok()
}

fn which(bin: &str) -> Option<String> {
    for dir in ["/usr/bin", "/usr/local/bin", "/bin"] {
        let p = Path::new(dir).join(bin);
        if p.is_file() {
            return Some(p.display().to_string());
        }
    }
    None
}

/// Probe the session. Never prompts: a diagnostic must not raise a portal
/// dialog as a side effect, so the remote-desktop session is not opened,
/// only its interface version read.
pub async fn doctor(state_dir: &Path) -> Doctor {
    let mut d = Doctor {
        session_type: std::env::var("XDG_SESSION_TYPE").unwrap_or_default(),
        desktop: std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default(),
        checks: Vec::new(),
    };

    let a11y =
        tokio::time::timeout(std::time::Duration::from_secs(5), crate::a11y::connect()).await;
    match a11y {
        Ok(Ok(conn)) => {
            let apps = crate::a11y::applications(&conn).await.unwrap_or_default();
            let n = apps.iter().filter(|a| !crate::a11y::is_system_ui(&a.name)).count();
            d.checks.push(Check {
                name: "accessibility",
                ok: true,
                detail: format!("bus reachable, {n} application(s) exporting a tree"),
                fix: "",
            });
        }
        Ok(Err(e)) => d.checks.push(Check {
            name: "accessibility",
            ok: false,
            detail: e,
            fix: "install at-spi2-core and log into a graphical session; without the bus get_ui_tree and ui_action return nothing",
        }),
        Err(_) => d.checks.push(Check {
            name: "accessibility",
            ok: false,
            detail: "timed out reaching org.a11y.Bus".into(),
            fix: "is at-spi-bus-launcher running in this session?",
        }),
    }
    match atspi::connection::read_session_accessibility().await {
        Ok(v) => d.checks.push(Check {
            name: "a11y enabled",
            ok: v,
            detail: if v { "toolkits export trees".into() } else { "org.a11y.Status IsEnabled is false".into() },
            fix: "agentctl turns this on when it starts; GTK3 and Electron apps export no tree until then",
        }),
        Err(e) => d.checks.push(Check {
            name: "a11y enabled",
            ok: false,
            detail: format!("cannot read org.a11y.Status: {e}"),
            fix: "",
        }),
    }

    for (iface, name, fix) in [
        ("RemoteDesktop", "input portal", "install xdg-desktop-portal and a backend (xdg-desktop-portal-gnome, -kde or -wlr); without it keyboard_type, mouse_action and every other synthetic input is refused"),
        ("ScreenCast", "screencast portal", "absolute pointer motion needs a monitor stream from this portal"),
        ("Screenshot", "capture portal", "capture_screen and ocr_region need this portal"),
    ] {
        match portal_version(iface).await {
            Some(v) if iface != "RemoteDesktop" || v >= 1 => d.checks.push(Check {
                name,
                ok: true,
                detail: format!("org.freedesktop.portal.{iface} version {v}"),
                fix: "",
            }),
            Some(v) => d.checks.push(Check { name, ok: false, detail: format!("version {v} is too old"), fix }),
            None => d.checks.push(Check { name, ok: false, detail: "not on the session bus".into(), fix }),
        }
    }

    let token = state_dir.join("portal-restore-token");
    d.checks.push(Check {
        name: "input approval",
        ok: token.is_file(),
        detail: if token.is_file() {
            format!("remembered ({})", token.display())
        } else {
            "not yet granted".into()
        },
        fix: "the first synthetic-input call raises a system dialog; approve it once and the grant is remembered",
    });

    // The Wayland clipboard needs a protocol Mutter lacks; note whether a
    // bridge exists.
    let x11_clip = crate::clip::X11Tool::detect();
    d.checks.push(Check {
        name: "clipboard",
        ok: true,
        detail: match &x11_clip {
            Some((_, bin)) => format!("wl-clipboard, with {bin} as an XWayland fallback"),
            None => "wl-clipboard only; on GNOME this fails (no data-control), install xclip or xsel to bridge it".into(),
        },
        fix: "",
    });

    // Human takeover needs a global pointer query. X11 has one; Wayland does
    // not, and without it moving the mouse will not stop the agent.
    match crate::pointer::support() {
        crate::pointer::PointerSupport::X11 { xdotool } => d.checks.push(Check {
            name: "human takeover",
            ok: true,
            detail: format!("pointer readable through {xdotool}"),
            fix: "",
        }),
        crate::pointer::PointerSupport::Unavailable(why) => d.checks.push(Check {
            name: "human takeover",
            ok: false,
            detail: format!("detection is OFF: {why}"),
            fix: "moving the mouse will not stop the agent here; the STOP file still does",
        }),
    }

    let consent = which("zenity").or_else(|| which("notify-send"));
    d.checks.push(Check {
        name: "consent dialog",
        ok: consent.is_some(),
        detail: consent
            .clone()
            .unwrap_or_else(|| "neither zenity nor notify-send found".into()),
        fix: "install zenity; without a dialog every action that needs approval is denied",
    });

    let ocr_dir = crate::vision::Ocr::model_dir(state_dir);
    let models = crate::vision::Ocr::models_present(&ocr_dir);
    d.checks.push(Check {
        name: "ocr models",
        ok: models,
        detail: if models { format!("present ({})", ocr_dir.display()) } else { "not downloaded yet".into() },
        fix: "the first ocr_region call downloads them (about 20 MB) with curl; place text-detection.onnx and text-recognition.onnx there by hand on an offline machine, or point AGENTCTL_OCR_MODELS at a directory that has them",
    });

    for (bin, name, fix) in [
        (
            "gtk-launch",
            "launcher",
            "install gtk4 (gtk-launch); launch and focus_app need it",
        ),
        ("espeak-ng", "speech", "install espeak-ng for speak"),
        (
            "wpctl",
            "audio control",
            "install wireplumber for the volume setting",
        ),
        (
            "curl",
            "downloader",
            "install curl, used once to fetch the OCR models",
        ),
    ] {
        let found = which(bin);
        d.checks.push(Check {
            name,
            ok: found.is_some(),
            detail: found.unwrap_or_else(|| format!("{bin} not found")),
            fix,
        });
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_lines_name_every_check_and_only_failed_fixes() {
        let d = Doctor {
            session_type: "wayland".into(),
            desktop: "GNOME".into(),
            checks: vec![
                Check {
                    name: "accessibility",
                    ok: true,
                    detail: "bus reachable".into(),
                    fix: "irrelevant",
                },
                Check {
                    name: "consent dialog",
                    ok: false,
                    detail: "none".into(),
                    fix: "install zenity",
                },
                Check {
                    name: "quiet",
                    ok: false,
                    detail: "x".into(),
                    fix: "",
                },
            ],
        };
        let lines = d.lines();
        assert_eq!(lines[0], "  session:         wayland (GNOME)");
        assert!(lines[1].contains("accessibility:") && lines[1].contains("ok"));
        assert!(!lines.iter().any(|l| l.contains("irrelevant")));
        assert!(lines.iter().any(|l| l.trim() == "-> install zenity"));
        assert_eq!(
            lines.len(),
            5,
            "a failed check with no fix adds no arrow line"
        );
        let empty = Doctor::default().lines();
        assert_eq!(
            empty,
            vec!["  session:         unknown (no XDG_CURRENT_DESKTOP)"]
        );
    }

    #[test]
    fn which_finds_real_binaries_and_not_imaginary_ones() {
        assert!(which("sh").is_some());
        assert!(which("definitely-not-a-binary-xyz").is_none());
    }
}
