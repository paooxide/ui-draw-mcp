//! Linux backend for `network_manage`: NetworkManager through `nmcli`.
//!
//! Terse mode (`-t`) is the machine-readable form: one record per line,
//! fields separated by `:`, with a literal `:` in a value escaped as `\:`.
//! Everything below parses that form and nothing else, and the Wi-Fi secret
//! goes to `nmcli` as argv, never into a log line or a response.

use std::path::Path;

use mcp_types::{Envelope, ErrorCode};
use serde_json::{json, Value};

use crate::tools::{run_net_tool, NetModule};

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

/// Split one `nmcli -t` line on unescaped `:`; `\:` and `\\` are unescaped.
pub(crate) fn split_terse(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(n) => cur.push(n),
                None => cur.push('\\'),
            },
            ':' => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// A row of `nmcli -t -f DEVICE,TYPE,STATE,CONNECTION dev status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Device {
    pub device: String,
    pub kind: String,
    pub state: String,
    pub connection: String,
}

pub(crate) fn parse_dev_status(text: &str) -> Vec<Device> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| {
            let f = split_terse(l);
            (f.len() >= 4 && !f[0].is_empty()).then(|| Device {
                device: f[0].clone(),
                kind: f[1].clone(),
                state: f[2].clone(),
                connection: f[3].clone(),
            })
        })
        .collect()
}

/// A row of `nmcli -t -f IN-USE,SSID,SIGNAL,SECURITY dev wifi list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Network {
    pub in_use: bool,
    pub ssid: String,
    pub signal: u8,
    pub security: String,
}

/// Visible networks, one per SSID (an SSID broadcast by several access points
/// keeps its strongest reading), hidden SSIDs dropped.
pub(crate) fn parse_wifi_list(text: &str) -> Vec<Network> {
    let mut out: Vec<Network> = Vec::new();
    for l in text.lines().filter(|l| !l.trim().is_empty()) {
        let f = split_terse(l);
        if f.len() < 4 || f[1].is_empty() {
            continue;
        }
        let n = Network {
            in_use: f[0].trim() == "*",
            ssid: f[1].clone(),
            signal: f[2].trim().parse().unwrap_or(0),
            security: f[3].trim().to_string(),
        };
        match out.iter_mut().find(|e| e.ssid == n.ssid) {
            Some(e) => {
                e.in_use |= n.in_use;
                if n.signal > e.signal {
                    e.signal = n.signal;
                    e.security = n.security;
                }
            }
            None => out.push(n),
        }
    }
    out
}

/// A row of `nmcli -t -f NAME,TYPE,DEVICE,STATE con show`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Connection {
    pub name: String,
    pub kind: String,
    pub device: String,
    pub state: String,
}

pub(crate) fn parse_con_show(text: &str) -> Vec<Connection> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| {
            let f = split_terse(l);
            (f.len() >= 4 && !f[0].is_empty()).then(|| Connection {
                name: f[0].clone(),
                kind: f[1].clone(),
                device: f[2].clone(),
                state: f[3].clone(),
            })
        })
        .collect()
}

/// Connection types that carry a tunnel: what `scutil --nc list` would show.
pub(crate) fn is_vpn_type(kind: &str) -> bool {
    matches!(kind, "vpn" | "wireguard" | "tun" | "ip-tunnel")
}

/// Replace every occurrence of `secret` in `text` so a tool's own echo of its
/// arguments can never carry the password out.
pub(crate) fn scrub(text: &str, secret: &str) -> String {
    if secret.is_empty() {
        return text.to_string();
    }
    text.replace(secret, "***")
}

async fn nmcli(args: &[&str]) -> Result<String, String> {
    let nmcli = linux_tool("nmcli")?;
    run_net_tool(&nmcli, args).await
}

/// The first Wi-Fi device NetworkManager manages.
pub(crate) async fn wifi_device() -> Option<Device> {
    let text = nmcli(&["-t", "-f", "DEVICE,TYPE,STATE,CONNECTION", "dev", "status"])
        .await
        .ok()?;
    parse_dev_status(&text)
        .into_iter()
        .find(|d| d.kind == "wifi")
}

impl NetModule {
    pub(crate) async fn linux_network_manage(&self, args: &Value) -> Envelope {
        let tool = "network_manage";
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("list_wifi");
        if let Err(e) = linux_tool("nmcli") {
            return Envelope::fail_with(
                tool,
                ErrorCode::ActionFailed,
                e,
                "NetworkManager's nmcli is what this tool drives on Linux",
            );
        }
        match action {
            "list_wifi" => {
                let Some(dev) = wifi_device().await else {
                    return Envelope::fail(
                        tool,
                        ErrorCode::NotFound,
                        "no Wi-Fi interface on this machine",
                    );
                };
                let visible = nmcli(&[
                    "-t",
                    "-f",
                    "IN-USE,SSID,SIGNAL,SECURITY",
                    "dev",
                    "wifi",
                    "list",
                    "--rescan",
                    "no",
                    "ifname",
                    &dev.device,
                ])
                .await
                .map(|t| parse_wifi_list(&t))
                .unwrap_or_default();
                let current = visible
                    .iter()
                    .find(|n| n.in_use)
                    .map(|n| n.ssid.clone())
                    .unwrap_or_else(|| {
                        if dev.state.starts_with("connected") {
                            dev.connection.clone()
                        } else {
                            String::new()
                        }
                    });
                // Saved profiles are the analogue of macOS's preferred list.
                let preferred: Vec<String> =
                    nmcli(&["-t", "-f", "NAME,TYPE,DEVICE,STATE", "con", "show"])
                        .await
                        .map(|t| {
                            parse_con_show(&t)
                                .into_iter()
                                .filter(|c| c.kind == "802-11-wireless")
                                .map(|c| c.name)
                                .collect()
                        })
                        .unwrap_or_default();
                let visible: Vec<Value> = visible
                    .iter()
                    .map(|n| {
                        json!({ "ssid": n.ssid, "signal": n.signal,
                                "security": n.security, "in_use": n.in_use })
                    })
                    .collect();
                Envelope::ok(
                    tool,
                    json!({
                        "device": dev.device, "state": dev.state,
                        "current": current,
                        "preferred": preferred, "count": preferred.len(),
                        "visible": visible,
                    }),
                )
            }
            "vpn_status" => {
                let text = match nmcli(&["-t", "-f", "NAME,TYPE,DEVICE,STATE", "con", "show"]).await
                {
                    Ok(t) => t,
                    Err(e) => return Envelope::fail(tool, ErrorCode::ActionFailed, e),
                };
                let rows: Vec<Value> = parse_con_show(&text)
                    .into_iter()
                    .filter(|c| is_vpn_type(&c.kind))
                    .map(|c| {
                        let connected = c.state == "activated";
                        json!({
                            "entry": format!("{} ({}) ({})", c.name, c.kind,
                                             if connected { "Connected" } else { "Disconnected" }),
                            "name": c.name, "type": c.kind, "device": c.device,
                            "connected": connected,
                        })
                    })
                    .collect();
                Envelope::ok(tool, json!({ "services": rows, "count": rows.len() }))
            }
            "connect" => {
                let Some(dev) = wifi_device().await else {
                    return Envelope::fail(tool, ErrorCode::NotFound, "no Wi-Fi interface");
                };
                let Some(ssid) = args.get("ssid").and_then(Value::as_str) else {
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, "'connect' needs 'ssid'");
                };
                if ssid.is_empty() || ssid.starts_with('-') || ssid.len() > 64 {
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, "invalid 'ssid'");
                }
                let secret = args.get("secret").and_then(Value::as_str).unwrap_or("");
                let mut argv = vec![
                    "dev",
                    "wifi",
                    "connect",
                    ssid,
                    "ifname",
                    dev.device.as_str(),
                ];
                if !secret.is_empty() {
                    argv.push("password");
                    argv.push(secret);
                }
                // nmcli does not repeat the password in its messages, and the
                // scrub makes sure of it either way.
                match nmcli(&argv).await {
                    Ok(out) => Envelope::ok(
                        tool,
                        json!({ "ok": out.contains("successfully activated"), "ssid": ssid,
                                "detail": scrub(out.trim(), secret) }),
                    ),
                    Err(e) => Envelope::fail(tool, ErrorCode::ActionFailed, scrub(&e, secret)),
                }
            }
            "disconnect" => {
                let Some(dev) = wifi_device().await else {
                    return Envelope::fail(tool, ErrorCode::NotFound, "no Wi-Fi interface");
                };
                match nmcli(&["radio", "wifi", "off"]).await {
                    Ok(_) => Envelope::ok(
                        tool,
                        json!({ "ok": true, "device": dev.device, "power": "off" }),
                    ),
                    Err(e) => Envelope::fail(tool, ErrorCode::ActionFailed, e),
                }
            }
            other => Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                format!("unknown action '{other}' (list_wifi|connect|disconnect|vpn_status)"),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terse_lines_split_on_unescaped_colons_only() {
        assert_eq!(split_terse("a:b:c"), ["a", "b", "c"]);
        assert_eq!(
            split_terse("wlp2s0:wifi:connected:Bumblebee"),
            ["wlp2s0", "wifi", "connected", "Bumblebee"]
        );
        assert_eq!(split_terse("AA\\:BB\\:CC:x"), ["AA:BB:CC", "x"]);
        assert_eq!(split_terse("back\\\\slash:y"), ["back\\slash", "y"]);
        assert_eq!(split_terse(""), [""]);
        assert_eq!(split_terse(":"), ["", ""]);
        // A trailing backslash is kept rather than dropped.
        assert_eq!(split_terse("a\\"), ["a\\"]);
    }

    const DEV_STATUS: &str = "wlp2s0:wifi:connected:Bumblebee\nlo:loopback:connected (externally):lo\ntailscale0:tun:connected (externally):tailscale0\np2p-dev-wlp2s0:wifi-p2p:disconnected:\neno1:ethernet:unavailable:\n";

    #[test]
    fn dev_status_finds_the_wifi_device() {
        let d = parse_dev_status(DEV_STATUS);
        assert_eq!(d.len(), 5);
        let wifi = d.iter().find(|d| d.kind == "wifi").unwrap();
        assert_eq!(wifi.device, "wlp2s0");
        assert_eq!(wifi.state, "connected");
        assert_eq!(wifi.connection, "Bumblebee");
        // wifi-p2p is not a Wi-Fi interface.
        assert_eq!(d.iter().filter(|d| d.kind == "wifi").count(), 1);
        assert!(parse_dev_status("").is_empty());
        assert!(parse_dev_status("garbage\n\n").is_empty());
        assert!(
            parse_dev_status(":wifi:connected:x").is_empty(),
            "no device name"
        );
    }

    const WIFI_LIST: &str = " :Bumblebee:55:WPA2\n*:Bumblebee:61:WPA2\n ::47:WPA2\n :Caf\\: Wi-Fi:45:WPA2 WPA3\n :vodafone6B7544:39:WPA2\n";

    #[test]
    fn wifi_list_dedupes_ssids_and_drops_hidden_ones() {
        let n = parse_wifi_list(WIFI_LIST);
        let names: Vec<&str> = n.iter().map(|n| n.ssid.as_str()).collect();
        assert_eq!(names, ["Bumblebee", "Caf: Wi-Fi", "vodafone6B7544"]);
        assert!(n[0].in_use);
        assert_eq!(n[0].signal, 61, "strongest reading wins");
        assert_eq!(n[1].security, "WPA2 WPA3");
        assert!(parse_wifi_list("").is_empty());
        assert!(parse_wifi_list("*:x").is_empty(), "too few fields");
        assert_eq!(
            parse_wifi_list(" :x:lots:WPA2")[0].signal,
            0,
            "bad signal is zero, not a panic"
        );
    }

    const CON_SHOW: &str = "Bumblebee:802-11-wireless:wlp2s0:activated\nlo:loopback:lo:activated\ntailscale0:tun:tailscale0:activated\nWork VPN:vpn::\nwg0:wireguard:wg0:activated\nWired connection 1:802-3-ethernet::\n";

    #[test]
    fn con_show_separates_saved_wifi_from_tunnels() {
        let c = parse_con_show(CON_SHOW);
        assert_eq!(c.len(), 6);
        let wifi: Vec<&str> = c
            .iter()
            .filter(|c| c.kind == "802-11-wireless")
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(wifi, ["Bumblebee"]);
        let vpn: Vec<(&str, bool)> = c
            .iter()
            .filter(|c| is_vpn_type(&c.kind))
            .map(|c| (c.name.as_str(), c.state == "activated"))
            .collect();
        assert_eq!(
            vpn,
            [("tailscale0", true), ("Work VPN", false), ("wg0", true)]
        );
        assert!(parse_con_show("").is_empty());
        assert!(parse_con_show("only:two").is_empty());
    }

    #[test]
    fn scrub_removes_every_copy_of_the_secret() {
        assert_eq!(
            scrub("pw is hunter2, again hunter2", "hunter2"),
            "pw is ***, again ***"
        );
        assert_eq!(scrub("nothing here", "hunter2"), "nothing here");
        assert_eq!(scrub("keep", ""), "keep");
    }

    #[test]
    fn linux_tool_names_the_missing_binary() {
        let e = linux_tool("definitely-not-a-real-tool-xyz").unwrap_err();
        assert!(e.contains("definitely-not-a-real-tool-xyz"), "{e}");
        assert!(linux_tool("sh").is_ok());
    }
}
