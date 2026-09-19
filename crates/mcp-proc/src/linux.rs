//! Linux backend for `service_control`: systemd through `systemctl`.
//!
//! Two managers are consulted, the user's (`systemctl --user`) and the
//! system's. Reads cover both; a mutation goes to whichever manager knows the
//! unit, user first, and a system unit needs polkit's say-so, which is
//! reported as a failure rather than waited for.

use std::collections::BTreeMap;
use std::path::Path;

use mcp_types::{Envelope, ErrorCode};
use serde_json::{json, Value};

use crate::tools::{clip, valid_service_label, ProcModule};

/// Where a tool this engine shells out to is expected to live. Absolute paths
/// keep an agent-controlled `PATH` out of the picture; `/usr/bin` first because
/// merged-usr distributions put everything there, `/bin` for the ones that do
/// not.
pub(crate) fn linux_tool(name: &str) -> Result<String, String> {
    for dir in ["/usr/bin", "/bin"] {
        let p = Path::new(dir).join(name);
        if p.is_file() {
            return Ok(p.display().to_string());
        }
    }
    Err(format!(
        "{name} is not installed (looked in /usr/bin and /bin)"
    ))
}

/// `systemctl` with a scrubbed environment that still carries what the user
/// manager needs: `XDG_RUNTIME_DIR` (or the bus address) is how `--user`
/// finds its bus.
async fn systemctl(scope: Scope, args: &[&str]) -> Result<String, String> {
    let bin = linux_tool("systemctl")?;
    let mut cmd = tokio::process::Command::new(&bin);
    cmd.env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("HOME", std::env::var("HOME").unwrap_or_default())
        .env("SYSTEMD_PAGER", "")
        .env("SYSTEMD_COLORS", "0")
        .stdin(std::process::Stdio::null());
    for key in ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"] {
        if let Some(v) = std::env::var_os(key) {
            cmd.env(key, v);
        }
    }
    if scope == Scope::User {
        cmd.arg("--user");
    }
    cmd.arg("--no-ask-password").args(args);
    let out = cmd.output().await.map_err(|e| format!("systemctl: {e}"))?;
    if !out.status.success() {
        let e = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if e.is_empty() {
            format!("systemctl exited {}", out.status)
        } else {
            e
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    User,
    System,
}

impl Scope {
    fn as_str(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::System => "system",
        }
    }
}

/// `list-units --output=json`: `[{unit, load, active, sub, description}]`.
pub(crate) fn parse_units_json(text: &str, scope: &str) -> Result<Vec<Value>, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
    let Some(arr) = v.as_array() else {
        return Err("list-units JSON is not an array".into());
    };
    Ok(arr.iter().filter_map(|u| unit_row(u, scope)).collect())
}

fn unit_row(u: &Value, scope: &str) -> Option<Value> {
    let s = |k: &str| u.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let label = s("unit");
    if label.is_empty() {
        return None;
    }
    let active = s("active");
    Some(json!({
        "label": label,
        "scope": scope,
        "load": s("load"),
        "active": active,
        "sub": s("sub"),
        "running": active == "active",
        "description": s("description"),
    }))
}

/// `list-units --no-legend --plain`: columns `UNIT LOAD ACTIVE SUB DESCRIPTION`,
/// the description free text to the end of the line. Older systemd has no
/// JSON output, so this is the fallback.
pub(crate) fn parse_units_plain(text: &str, scope: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let unit = f.next()?;
            let load = f.next()?;
            let active = f.next()?;
            let sub = f.next()?;
            let description = f.collect::<Vec<_>>().join(" ");
            unit_row(
                &json!({ "unit": unit, "load": load, "active": active, "sub": sub,
                         "description": description }),
                scope,
            )
        })
        .collect()
}

/// `systemctl show -p ...`: `Key=Value` lines.
pub(crate) fn parse_show(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

/// A bare name means a service; anything with a unit suffix is taken as is.
pub(crate) fn unit_name(name: &str) -> String {
    const SUFFIXES: [&str; 11] = [
        ".service",
        ".socket",
        ".timer",
        ".target",
        ".mount",
        ".automount",
        ".path",
        ".slice",
        ".scope",
        ".swap",
        ".device",
    ];
    if SUFFIXES.iter().any(|s| name.ends_with(s)) {
        name.to_string()
    } else {
        format!("{name}.service")
    }
}

const SHOW_PROPS: &str =
    "Id,LoadState,ActiveState,SubState,MainPID,Description,Result,ExecMainStatus";

/// Which manager knows this unit, with its properties.
async fn find_unit(unit: &str) -> Option<(Scope, BTreeMap<String, String>)> {
    for scope in [Scope::User, Scope::System] {
        let Ok(text) = systemctl(scope, &["show", "-p", SHOW_PROPS, "--", unit]).await else {
            continue;
        };
        let props = parse_show(&text);
        if props.get("LoadState").is_some_and(|s| s != "not-found") {
            return Some((scope, props));
        }
    }
    None
}

async fn list_scope(scope: Scope) -> Vec<Value> {
    let json = systemctl(
        scope,
        &["list-units", "--type=service", "--all", "--output=json"],
    )
    .await;
    if let Ok(text) = &json {
        if let Ok(rows) = parse_units_json(text, scope.as_str()) {
            return rows;
        }
    }
    match systemctl(
        scope,
        &[
            "list-units",
            "--type=service",
            "--all",
            "--no-legend",
            "--plain",
        ],
    )
    .await
    {
        Ok(text) => parse_units_plain(&text, scope.as_str()),
        Err(e) => {
            tracing::debug!("systemctl {} list-units unavailable: {e}", scope.as_str());
            Vec::new()
        }
    }
}

impl ProcModule {
    pub(crate) async fn linux_service_control(&self, args: &Value) -> Envelope {
        let tool = "service_control";
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("status");
        if let Err(e) = linux_tool("systemctl") {
            return Envelope::fail(tool, ErrorCode::UnsupportedOs, e);
        }
        if action == "list" {
            let mut rows = list_scope(Scope::User).await;
            rows.extend(list_scope(Scope::System).await);
            if rows.is_empty() {
                return Envelope::fail_with(
                    tool,
                    ErrorCode::ActionFailed,
                    "systemctl listed no units in either the user or the system manager",
                    "is systemd the init here, and is the D-Bus system bus reachable?",
                );
            }
            return Envelope::ok(tool, json!({ "services": rows, "count": rows.len() }));
        }
        let Some(name) = args
            .get("name")
            .and_then(Value::as_str)
            .filter(|n| !n.is_empty())
        else {
            return Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                "missing 'name' (a systemd unit)",
            );
        };
        if !valid_service_label(name) {
            return Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                format!("'{name}' is not a valid unit name"),
            );
        }
        let unit = unit_name(name);
        let Some((scope, props)) = find_unit(&unit).await else {
            return Envelope::fail(
                tool,
                ErrorCode::NotFound,
                format!("no unit '{unit}' in the user or system manager"),
            );
        };
        let p = |k: &str| props.get(k).cloned().unwrap_or_default();
        match action {
            "status" => Envelope::ok(
                tool,
                json!({
                    "name": name, "unit": unit, "scope": scope.as_str(),
                    "running": p("ActiveState") == "active",
                    "active": p("ActiveState"), "sub": p("SubState"), "load": p("LoadState"),
                    "pid": p("MainPID").parse::<i64>().ok().filter(|n| *n > 0),
                    "last_exit": p("ExecMainStatus").parse::<i64>().ok(),
                    "result": p("Result"), "description": p("Description"),
                    "detail": clip(&props.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("\n"), 4000),
                }),
            ),
            verb @ ("start" | "stop" | "restart") => {
                match systemctl(scope, &[verb, "--", &unit]).await {
                    Ok(out) => Envelope::ok(
                        tool,
                        json!({ "ok": true, "name": name, "unit": unit,
                                "scope": scope.as_str(), "detail": out.trim() }),
                    ),
                    Err(e) => Envelope::fail_with(
                        tool,
                        ErrorCode::ActionFailed,
                        e,
                        "a system unit needs polkit authorisation, which agentctl does not \
                         obtain on its own",
                    ),
                }
            }
            other => Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                format!("unknown action '{other}' (list|status|start|stop|restart)"),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNITS_JSON: &str = r#"[{"unit":"at-spi-dbus-bus.service","load":"loaded","active":"active","sub":"running","description":"Accessibility services bus"},{"unit":"abrt-vmcore.service","load":"loaded","active":"inactive","sub":"dead","description":"ABRT kernel panic detection"}]"#;

    #[test]
    fn units_json_maps_to_rows() {
        let r = parse_units_json(UNITS_JSON, "user").unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0]["label"], "at-spi-dbus-bus.service");
        assert_eq!(r[0]["scope"], "user");
        assert_eq!(r[0]["running"], true);
        assert_eq!(r[1]["running"], false);
        assert_eq!(r[1]["sub"], "dead");
        assert_eq!(r[1]["description"], "ABRT kernel panic detection");
    }

    #[test]
    fn units_json_rejects_garbage_and_skips_nameless_rows() {
        assert!(parse_units_json("", "user").is_err());
        assert!(parse_units_json("{}", "user").is_err());
        assert!(parse_units_json("nope", "user").is_err());
        assert_eq!(parse_units_json("[]", "user").unwrap().len(), 0);
        assert_eq!(
            parse_units_json(r#"[{"load":"loaded"}]"#, "user")
                .unwrap()
                .len(),
            0
        );
    }

    const UNITS_PLAIN: &str = "at-spi-dbus-bus.service                             loaded    active   running Accessibility services bus\nabrt-vmcore.service   loaded    inactive dead    ABRT kernel panic detection\ndbus-:1.2-org.freedesktop.secrets@0.service loaded active running dbus-:1.2-org.freedesktop.secrets@0.service\n";

    #[test]
    fn units_plain_keeps_the_description_whole() {
        let r = parse_units_plain(UNITS_PLAIN, "system");
        assert_eq!(r.len(), 3);
        assert_eq!(r[0]["description"], "Accessibility services bus");
        assert_eq!(r[0]["running"], true);
        assert_eq!(r[1]["active"], "inactive");
        assert_eq!(r[2]["label"], "dbus-:1.2-org.freedesktop.secrets@0.service");
        assert!(parse_units_plain("", "system").is_empty());
        assert!(parse_units_plain("too few\n", "system").is_empty());
    }

    #[test]
    fn show_output_is_key_value() {
        let p = parse_show("Id=dbus-broker.service\nLoadState=loaded\nActiveState=active\nMainPID=3609\nDescription=D-Bus User Message Bus=yes\n");
        assert_eq!(p["Id"], "dbus-broker.service");
        assert_eq!(p["MainPID"], "3609");
        assert_eq!(
            p["Description"], "D-Bus User Message Bus=yes",
            "only the first '=' splits"
        );
        assert!(parse_show("").is_empty());
        assert!(parse_show("no equals here\n").is_empty());
    }

    #[test]
    fn bare_names_become_services() {
        assert_eq!(unit_name("sshd"), "sshd.service");
        assert_eq!(unit_name("sshd.service"), "sshd.service");
        assert_eq!(unit_name("cups.socket"), "cups.socket");
        assert_eq!(unit_name("fstrim.timer"), "fstrim.timer");
        assert_eq!(unit_name("home.mount"), "home.mount");
    }

    #[test]
    fn linux_tool_names_the_missing_binary() {
        let e = linux_tool("definitely-not-a-real-tool-xyz").unwrap_err();
        assert!(e.contains("definitely-not-a-real-tool-xyz"), "{e}");
        assert!(linux_tool("sh").is_ok());
    }
}
