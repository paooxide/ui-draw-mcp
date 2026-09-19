use async_trait::async_trait;
use mcp_types::{CallCtx, Category, Envelope, ErrorCode, Tier, ToolDescriptor, ToolModule};
use serde_json::{json, Value};

use mcp_ssrf::{NetPolicy, UrlError};

/// Network tools: `http_request`, `dns_lookup`, `network_interfaces`.
///
/// HTTP goes through `curl` rather than a Rust TLS stack: it is present on
/// every target platform and avoids pulling a large TLS dependency into a
/// disk-constrained build. Arguments are passed as argv (never a shell string),
/// and redirects are **not** followed: a redirect is the standard way to turn
/// an allowlisted URL into an internal one.
pub struct NetModule {
    policy: NetPolicy,
    timeout_secs: u64,
    max_body_bytes: usize,
}

impl NetModule {
    pub fn new(policy: NetPolicy, timeout_secs: u64, max_body_bytes: usize) -> Self {
        NetModule {
            policy,
            timeout_secs,
            max_body_bytes,
        }
    }

    fn url_err(tool: &str, e: UrlError) -> Envelope {
        let code = match e {
            UrlError::Malformed(_) | UrlError::Scheme(_) => ErrorCode::InvalidArgs,
            UrlError::HostNotAllowed(_) | UrlError::BlockedAddress(_) => ErrorCode::PolicyDenied,
            UrlError::Unresolvable(_) => ErrorCode::NotFound,
        };
        Envelope::fail(tool, code, e.message())
    }

    async fn http_request(&self, args: &Value) -> Envelope {
        let tool = "http_request";
        let Some(url) = args.get("url").and_then(Value::as_str) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'url'");
        };
        let method = args
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("GET")
            .to_ascii_uppercase();
        if !matches!(
            method.as_str(),
            "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE"
        ) {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "unsupported method");
        }

        let (parsed, addrs) = match self.policy.check(url) {
            Ok(x) => x,
            Err(e) => return Self::url_err(tool, e),
        };

        let mut cmd = std::process::Command::new("/usr/bin/curl");
        cmd.arg("--silent")
            .arg("--show-error")
            .arg("--no-buffer")
            // Do NOT follow redirects: an allowlisted URL could redirect to an
            // internal one, and the guard only ran on the original.
            .arg("--max-redirs")
            .arg("0")
            .arg("--max-time")
            .arg(self.timeout_secs.to_string())
            .arg("--max-filesize")
            .arg(self.max_body_bytes.to_string())
            // Pin to an address we already validated, closing the gap between
            // our DNS check and curl's own resolution (DNS rebinding).
            .arg("--resolve")
            .arg(format!(
                "{}:{}:{}",
                parsed.host,
                parsed.port,
                addrs
                    .iter()
                    .map(|a| a.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            ))
            .arg("-w")
            .arg("\n%{http_code}")
            .arg("-X")
            .arg(&method);

        if let Some(headers) = args.get("headers").and_then(Value::as_object) {
            for (k, v) in headers {
                if let Some(v) = v.as_str() {
                    cmd.arg("-H").arg(format!("{k}: {v}"));
                }
            }
        }
        if let Some(body) = args.get("body").and_then(Value::as_str) {
            cmd.arg("--data-binary").arg(body);
        }
        cmd.arg("--").arg(url);

        let out = match cmd.output() {
            Ok(o) => o,
            Err(e) => return Envelope::fail(tool, ErrorCode::ActionFailed, e.to_string()),
        };
        if !out.status.success() {
            return Envelope::fail(
                tool,
                ErrorCode::ActionFailed,
                format!(
                    "curl failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            );
        }
        let raw = String::from_utf8_lossy(&out.stdout).to_string();
        let (body, status) = match raw.rsplit_once('\n') {
            Some((b, s)) => (b.to_string(), s.trim().parse::<u16>().unwrap_or(0)),
            None => (raw, 0),
        };
        let truncated = body.len() >= self.max_body_bytes;
        Envelope::ok(
            tool,
            json!({
                "url": url, "method": method, "status": status,
                "body": body, "bytes": body.len(), "truncated": truncated,
                "resolved": addrs.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
            }),
        )
    }

    /// Listening and established sockets with their owning process.
    ///
    /// `lsof` rather than `netstat`, because the owning PID is the part that
    /// makes this useful: "something is on 8080" is a much weaker answer than
    /// "node, pid 4711, is on 8080".
    async fn socket_inspection(&self, args: &Value) -> Envelope {
        let tool = "socket_inspection";
        let proto = args.get("proto").and_then(Value::as_str).unwrap_or("all");
        if !matches!(proto, "all" | "tcp" | "udp") {
            return Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                format!("unknown proto '{proto}' (all|tcp|udp)"),
            );
        }
        let want_state = args.get("state").and_then(Value::as_str);
        // -n/-P: no reverse DNS, no port-name lookup. Both are slow and neither
        // adds anything an agent can use.
        let selector = match proto {
            "tcp" => "-iTCP",
            "udp" => "-iUDP",
            _ => "-i",
        };
        let text = match run_net_tool("/usr/sbin/lsof", &["-nP", selector]).await {
            Ok(t) => t,
            Err(e) => return Envelope::fail(tool, ErrorCode::ActionFailed, e),
        };
        let mut rows = Vec::new();
        for line in text.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 9 {
                continue;
            }
            let name = f[8];
            let state = f.get(9).map(|s| s.trim_matches(['(', ')'])).unwrap_or("");
            if let Some(w) = want_state {
                if !state.eq_ignore_ascii_case(w) {
                    continue;
                }
            }
            rows.push(json!({
                "command": f[0],
                "pid": f[1].parse::<i64>().ok(),
                "user": f[2],
                "proto": f[7],
                "address": name,
                "state": state,
                "listening": state.eq_ignore_ascii_case("LISTEN") || name.contains("*:"),
            }));
            if rows.len() >= 500 {
                break;
            }
        }
        Envelope::ok(
            tool,
            json!({ "sockets": rows, "count": rows.len(), "proto": proto }),
        )
    }

    /// Reachability probes. Bounded in count and time so this cannot become a
    /// flood: `ping` is capped at a handful of packets and every op has a hard
    /// deadline.
    async fn packet_diagnostics(&self, args: &Value) -> Envelope {
        let tool = "packet_diagnostics";
        let op = args.get("op").and_then(Value::as_str).unwrap_or("ping");
        let Some(target) = args.get("target").and_then(Value::as_str) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'target'");
        };
        if !valid_host(target) {
            return Envelope::fail_with(
                tool,
                ErrorCode::InvalidArgs,
                format!("'{target}' is not a hostname or IP"),
                "pass a bare host like example.com or 10.0.0.1",
            );
        }
        // Probing is still reaching out to a host: the same allowlist and
        // private-range rules that gate http_request apply here.
        if let Err(e) = self.policy.check_host(target) {
            return Envelope::fail(tool, ErrorCode::PolicyDenied, e);
        }
        let count = args
            .get("count")
            .and_then(Value::as_u64)
            .unwrap_or(3)
            .clamp(1, 10);
        let (program, argv): (&str, Vec<String>) = match op {
            "ping" => (
                "/sbin/ping",
                vec![
                    "-c".into(),
                    count.to_string(),
                    "-W".into(),
                    "2000".into(),
                    target.to_string(),
                ],
            ),
            "traceroute" => (
                "/usr/sbin/traceroute",
                vec![
                    "-n".into(),
                    "-w".into(),
                    "2".into(),
                    "-m".into(),
                    "20".into(),
                    target.to_string(),
                ],
            ),
            "dns" => ("/usr/bin/dig", vec!["+short".into(), target.to_string()]),
            "tcp_connect" => {
                let port = args.get("port").and_then(Value::as_u64).unwrap_or(80);
                if !(1..=65535).contains(&port) {
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, "'port' must be 1-65535");
                }
                (
                    "/usr/bin/nc",
                    vec![
                        "-z".into(),
                        "-G".into(),
                        "3".into(),
                        "-w".into(),
                        "3".into(),
                        target.to_string(),
                        port.to_string(),
                    ],
                )
            }
            other => {
                return Envelope::fail(
                    tool,
                    ErrorCode::InvalidArgs,
                    format!("unknown op '{other}' (ping|traceroute|dns|tcp_connect)"),
                )
            }
        };
        let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        match run_net_timed(program, &refs, 45).await {
            Ok((ok, out)) => Envelope::ok(
                tool,
                json!({ "op": op, "target": target, "reachable": ok, "output": clip_net(&out, 8000) }),
            ),
            Err(e) => Envelope::fail(tool, ErrorCode::ActionFailed, e),
        }
    }

    /// The application firewall. Listing is a read; changing it is not, and on
    /// macOS the change also needs root, which this engine will not obtain for
    /// itself, so a denial from the OS is reported as exactly that.
    async fn firewall_rules(&self, args: &Value) -> Envelope {
        let tool = "firewall_rules";
        const FW: &str = "/usr/libexec/ApplicationFirewall/socketfilterfw";
        match args.get("action").and_then(Value::as_str).unwrap_or("list") {
            "list" => {
                let state = run_net_tool(FW, &["--getglobalstate"])
                    .await
                    .unwrap_or_default();
                let stealth = run_net_tool(FW, &["--getstealthmode"])
                    .await
                    .unwrap_or_default();
                let apps = run_net_tool(FW, &["--listapps"]).await.unwrap_or_default();
                let rules: Vec<Value> = apps
                    .lines()
                    .filter(|l| l.trim().starts_with(char::is_numeric) && l.contains(':'))
                    .map(|l| json!({ "entry": l.trim() }))
                    .collect();
                Envelope::ok(
                    tool,
                    json!({
                        "enabled": state.contains("enabled"),
                        "stealth_mode": stealth.contains("enabled"),
                        "apps": rules, "count": rules.len(),
                    }),
                )
            }
            action @ ("add" | "remove") => {
                let Some(rule) = args.get("rule").and_then(Value::as_str) else {
                    return Envelope::fail(
                        tool,
                        ErrorCode::InvalidArgs,
                        format!("'{action}' needs 'rule' (an application path)"),
                    );
                };
                if rule.starts_with('-') || !rule.starts_with('/') || rule.contains("..") {
                    return Envelope::fail_with(
                        tool,
                        ErrorCode::InvalidArgs,
                        format!("'{rule}' is not an absolute application path"),
                        "the firewall rules this manages are per-application, e.g. /Applications/Foo.app",
                    );
                }
                let flag = if action == "add" { "--add" } else { "--remove" };
                match run_net_tool(FW, &[flag, rule]).await {
                    Ok(out) => Envelope::ok(
                        tool,
                        json!({ "ok": true, "action": action, "rule": rule, "detail": out.trim() }),
                    ),
                    Err(e) => Envelope::fail_with(
                        tool,
                        ErrorCode::PermDenied,
                        e,
                        "changing the firewall needs root; agentctl does not escalate on its own",
                    ),
                }
            }
            other => Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                format!("unknown action '{other}' (list|add|remove)"),
            ),
        }
    }

    /// Wi-Fi and VPN state. The Wi-Fi password never enters a log line or a
    /// response: it goes to `networksetup` as argv and nowhere else.
    #[cfg(not(target_os = "linux"))]
    async fn network_manage(&self, args: &Value) -> Envelope {
        let tool = "network_manage";
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("list_wifi");
        match action {
            "list_wifi" => {
                let device = wifi_device().await;
                let Some(dev) = device else {
                    return Envelope::fail(
                        tool,
                        ErrorCode::NotFound,
                        "no Wi-Fi interface on this machine",
                    );
                };
                let current = run_net_tool("/usr/sbin/networksetup", &["-getairportnetwork", &dev])
                    .await
                    .unwrap_or_default();
                let preferred = run_net_tool(
                    "/usr/sbin/networksetup",
                    &["-listpreferredwirelessnetworks", &dev],
                )
                .await
                .unwrap_or_default();
                let networks: Vec<&str> = preferred
                    .lines()
                    .skip(1)
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .collect();
                Envelope::ok(
                    tool,
                    json!({
                        "device": dev,
                        "current": current.split(": ").nth(1).unwrap_or("").trim(),
                        "preferred": networks, "count": networks.len(),
                    }),
                )
            }
            "vpn_status" => {
                let text = match run_net_tool("/usr/sbin/scutil", &["--nc", "list"]).await {
                    Ok(t) => t,
                    Err(e) => return Envelope::fail(tool, ErrorCode::ActionFailed, e),
                };
                let rows: Vec<Value> = text
                    .lines()
                    .filter(|l| {
                        l.trim_start().starts_with('*')
                            || l.contains("(Connected)")
                            || l.contains("(Disconnected)")
                    })
                    .map(|l| json!({ "entry": l.trim(), "connected": l.contains("(Connected)") }))
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
                if ssid.starts_with('-') || ssid.len() > 64 {
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, "invalid 'ssid'");
                }
                let secret = args.get("secret").and_then(Value::as_str).unwrap_or("");
                let mut argv = vec!["-setairportnetwork", dev.as_str(), ssid];
                if !secret.is_empty() {
                    argv.push(secret);
                }
                match run_net_tool("/usr/sbin/networksetup", &argv).await {
                    // The response deliberately does not echo the secret back.
                    Ok(out) => Envelope::ok(
                        tool,
                        json!({ "ok": !out.contains("Failed"), "ssid": ssid, "detail": out.trim() }),
                    ),
                    Err(e) => Envelope::fail(tool, ErrorCode::ActionFailed, e),
                }
            }
            "disconnect" => {
                let Some(dev) = wifi_device().await else {
                    return Envelope::fail(tool, ErrorCode::NotFound, "no Wi-Fi interface");
                };
                match run_net_tool("/usr/sbin/networksetup", &["-setairportpower", &dev, "off"])
                    .await
                {
                    Ok(_) => {
                        Envelope::ok(tool, json!({ "ok": true, "device": dev, "power": "off" }))
                    }
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

    /// Bluetooth pairing.
    ///
    /// macOS ships no supported command-line pairing interface: `system_profiler`
    /// lists devices and nothing more. Where `blueutil` is installed it is used;
    /// otherwise the tool says exactly what is missing instead of failing vaguely.
    async fn bluetooth_pair(&self, args: &Value) -> Envelope {
        let tool = "bluetooth_pair";
        let action = args.get("action").and_then(Value::as_str).unwrap_or("list");
        if action == "list" {
            let text = run_net_tool("/usr/sbin/system_profiler", &["SPBluetoothDataType"])
                .await
                .unwrap_or_default();
            let devices: Vec<Value> = text
                .lines()
                .filter(|l| l.contains("Address:"))
                .map(|l| json!({ "address": l.split("Address:").nth(1).unwrap_or("").trim() }))
                .collect();
            return Envelope::ok(
                tool,
                json!({ "devices": devices, "count": devices.len(),
                        "note": "listing only; pairing needs blueutil" }),
            );
        }
        if !matches!(action, "pair" | "unpair" | "connect") {
            return Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                format!(
                    "unknown action '{other}' (list|pair|unpair|connect)",
                    other = action
                ),
            );
        }
        let Some(device) = args.get("device").and_then(Value::as_str) else {
            return Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                format!("'{action}' needs 'device'"),
            );
        };
        if !valid_bt_address(device) {
            return Envelope::fail_with(
                tool,
                ErrorCode::InvalidArgs,
                format!("'{device}' is not a Bluetooth address"),
                "pass a MAC-style address such as aa-bb-cc-dd-ee-ff",
            );
        }
        let Some(blueutil) = ["/opt/homebrew/bin/blueutil", "/usr/local/bin/blueutil"]
            .into_iter()
            .find(|p| std::path::Path::new(p).exists())
        else {
            return Envelope::fail_with(
                tool,
                ErrorCode::UnsupportedOs,
                "macOS has no supported command-line Bluetooth pairing interface",
                "install blueutil (brew install blueutil) to enable pair/unpair/connect; \
                 action=list works without it",
            );
        };
        let flag = match action {
            "pair" => "--pair",
            "unpair" => "--unpair",
            _ => "--connect",
        };
        match run_net_tool(blueutil, &[flag, device]).await {
            Ok(out) => Envelope::ok(
                tool,
                json!({ "ok": true, "action": action, "device": device, "detail": out.trim() }),
            ),
            Err(e) => Envelope::fail(tool, ErrorCode::ActionFailed, e),
        }
    }

    fn dns_lookup(&self, args: &Value) -> Envelope {
        let tool = "dns_lookup";
        let Some(host) = args.get("host").and_then(Value::as_str) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'host'");
        };
        use std::net::ToSocketAddrs;
        match (host, 80u16).to_socket_addrs() {
            Ok(it) => {
                let addrs: Vec<Value> = it
                    .map(|s| {
                        let ip = s.ip();
                        json!({ "ip": ip.to_string(), "private_or_internal": crate::is_blocked_ip(&ip) })
                    })
                    .collect();
                Envelope::ok(tool, json!({ "host": host, "addresses": addrs }))
            }
            Err(e) => Envelope::fail(tool, ErrorCode::NotFound, format!("{host}: {e}")),
        }
    }

    fn interfaces(&self) -> Envelope {
        let tool = "network_interfaces";
        let out = std::process::Command::new("/sbin/ifconfig")
            .arg("-a")
            .output();
        match out {
            Ok(o) if o.status.success() => {
                let text = String::from_utf8_lossy(&o.stdout);
                let mut ifaces = Vec::new();
                let mut current: Option<(String, Vec<String>)> = None;
                for line in text.lines() {
                    if !line.starts_with([' ', '\t']) {
                        if let Some((n, a)) = current.take() {
                            ifaces.push(json!({ "name": n, "addresses": a }));
                        }
                        if let Some(name) = line.split(':').next() {
                            current = Some((name.to_string(), Vec::new()));
                        }
                    } else if let Some((_, addrs)) = current.as_mut() {
                        let t = line.trim();
                        if let Some(rest) =
                            t.strip_prefix("inet ").or_else(|| t.strip_prefix("inet6 "))
                        {
                            if let Some(a) = rest.split_whitespace().next() {
                                addrs.push(a.to_string());
                            }
                        }
                    }
                }
                if let Some((n, a)) = current {
                    ifaces.push(json!({ "name": n, "addresses": a }));
                }
                Envelope::ok(tool, json!({ "interfaces": ifaces }))
            }
            Ok(_) | Err(_) => Envelope::fail(
                tool,
                ErrorCode::UnsupportedOs,
                "could not enumerate interfaces on this platform",
            ),
        }
    }
}

fn valid_host(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && !h.starts_with('-')
        && h.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '_'))
}

fn valid_bt_address(a: &str) -> bool {
    let clean: String = a.chars().filter(|c| *c != '-' && *c != ':').collect();
    clean.len() == 12 && clean.chars().all(|c| c.is_ascii_hexdigit())
}

fn clip_net(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[truncated]", &text[..end])
}

/// Run a fixed diagnostic binary. The program is always a literal in this file;
/// only arguments vary, and the caller validates those.
pub(crate) async fn run_net_tool(program: &str, args: &[&str]) -> Result<String, String> {
    run_net_timed(program, args, 30)
        .await
        .and_then(|(ok, out)| {
            if ok {
                Ok(out)
            } else {
                Err(out.trim().to_string())
            }
        })
}

/// Same, but reports success separately: a failing `ping` is a *result*
/// ("unreachable"), not an error.
async fn run_net_timed(
    program: &str,
    args: &[&str],
    timeout_secs: u64,
) -> Result<(bool, String), String> {
    let fut = tokio::process::Command::new(program)
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .stdin(std::process::Stdio::null())
        .output();
    let out = match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), fut).await {
        Err(_) => return Err(format!("{program} timed out")),
        Ok(Err(e)) => return Err(format!("{program}: {e}")),
        Ok(Ok(o)) => o,
    };
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    if text.trim().is_empty() {
        text = String::from_utf8_lossy(&out.stderr).to_string();
    }
    Ok((out.status.success(), text))
}

/// The Wi-Fi interface name (`en0` on most Macs, but not all).
#[cfg(not(target_os = "linux"))]
async fn wifi_device() -> Option<String> {
    let text = run_net_tool("/usr/sbin/networksetup", &["-listallhardwareports"])
        .await
        .ok()?;
    let mut lines = text.lines();
    while let Some(l) = lines.next() {
        if l.contains("Wi-Fi") || l.contains("AirPort") {
            for next in lines.by_ref() {
                if let Some(dev) = next.strip_prefix("Device: ") {
                    return Some(dev.trim().to_string());
                }
            }
        }
    }
    None
}

#[async_trait]
impl ToolModule for NetModule {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        vec![
            ToolDescriptor::new(
                "http_request",
                Category::Network,
                Tier::Dangerous,
                "Make an HTTP request to an allowlisted host. Private/loopback/metadata \
                 addresses are blocked and redirects are not followed.",
                json!({
                    "type": "object",
                    "properties": {
                        "url": { "type": "string" },
                        "method": { "type": "string", "enum": ["GET","HEAD","POST","PUT","PATCH","DELETE"] },
                        "headers": { "type": "object" },
                        "body": { "type": "string" }
                    },
                    "required": ["url"]
                }),
            ).untrusted_output(),
            ToolDescriptor::new(
                "dns_lookup",
                Category::Network,
                Tier::Read,
                "Resolve a hostname, flagging any private/internal addresses.",
                json!({ "type": "object", "properties": { "host": { "type": "string" } }, "required": ["host"] }),
            ),
            ToolDescriptor::new(
                "socket_inspection",
                Category::Network,
                Tier::Read,
                "Open sockets with the process that owns each one: what is listening on a port, \
                 and which program it is.",
                json!({"type":"object","properties":{
                    "proto":{"type":"string","enum":["all","tcp","udp"]},
                    "state":{"type":"string","description":"e.g. LISTEN, ESTABLISHED"}},
                    "required":[]}),
            ).open_world(false),
            ToolDescriptor::new(
                "packet_diagnostics",
                Category::Network,
                Tier::Standard,
                "Reachability probes: ping, traceroute, dns, tcp_connect. Subject to the same \
                 host allowlist and private-range rules as http_request: a probe is still a \
                 reach-out.",
                json!({"type":"object","properties":{
                    "op":{"type":"string","enum":["ping","traceroute","dns","tcp_connect"]},
                    "target":{"type":"string"},"port":{"type":"integer"},
                    "count":{"type":"integer","description":"ping packets, 1-10"}},
                    "required":["op","target"]}),
            ),
            ToolDescriptor::new(
                "firewall_rules",
                Category::Network,
                Tier::Dangerous,
                "Read the application firewall, or add/remove a per-application rule. Changes need \
                 root, which this server never obtains for itself.",
                json!({"type":"object","properties":{
                    "action":{"type":"string","enum":["list","add","remove"]},
                    "rule":{"type":"string","description":"absolute application path"}},
                    "required":["action"]}),
            ),
            ToolDescriptor::new(
                "network_manage",
                Category::Network,
                Tier::Dangerous,
                "Wi-Fi and VPN state. Connecting changes which network this machine is on, and \
                 therefore what it can reach and who can reach it. A supplied secret goes to the \
                 OS and is never echoed back or logged.",
                json!({"type":"object","properties":{
                    "action":{"type":"string","enum":["list_wifi","connect","disconnect","vpn_status"]},
                    "ssid":{"type":"string"},"secret":{"type":"string"}},
                    "required":["action"]}),
            ),
            ToolDescriptor::new(
                "bluetooth_pair",
                Category::Network,
                Tier::Dangerous,
                "List Bluetooth devices, or pair/unpair/connect one. Pairing grants a device \
                 standing access, so it is gated; listing is available everywhere, while pairing \
                 needs blueutil since macOS ships no supported CLI for it.",
                json!({"type":"object","properties":{
                    "action":{"type":"string","enum":["list","pair","unpair","connect"]},
                    "device":{"type":"string","description":"Bluetooth address"}},
                    "required":["action"]}),
            ),
            ToolDescriptor::new(
                "network_interfaces",
                Category::Network,
                Tier::Read,
                "List local network interfaces and their addresses.",
                json!({ "type": "object", "properties": {}, "required": [] }),
            ).open_world(false),
        ]
    }

    async fn call(&self, name: &str, args: Value, _ctx: &CallCtx) -> Envelope {
        match name {
            "http_request" => self.http_request(&args).await,
            "dns_lookup" => self.dns_lookup(&args),
            "network_interfaces" => self.interfaces(),
            "socket_inspection" => self.socket_inspection(&args).await,
            "packet_diagnostics" => self.packet_diagnostics(&args).await,
            "firewall_rules" => self.firewall_rules(&args).await,
            #[cfg(not(target_os = "linux"))]
            "network_manage" => self.network_manage(&args).await,
            #[cfg(target_os = "linux")]
            "network_manage" => self.linux_network_manage(&args).await,
            "bluetooth_pair" => self.bluetooth_pair(&args).await,
            other => Envelope::fail(other, ErrorCode::InvalidArgs, "unknown tool"),
        }
    }

    /// Requests that can change remote state, or carry a body, get a human in
    /// the loop: that is the exfiltration path.
    fn consent_prompt(&self, name: &str, args: &Value) -> Option<String> {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("");
        match name {
            // Reads are free everywhere; only the mutations ask.
            "firewall_rules" => {
                return matches!(action, "add" | "remove").then(|| {
                    format!(
                        "{action} a firewall rule for '{}'? This changes what this machine will \
                         accept or refuse from the network.",
                        args.get("rule").and_then(Value::as_str).unwrap_or("?")
                    )
                })
            }
            "network_manage" => {
                return match action {
                    "connect" => Some(format!(
                        "Join the Wi-Fi network '{}'? Changing network changes what this machine \
                         can reach and who can reach it.",
                        args.get("ssid").and_then(Value::as_str).unwrap_or("?")
                    )),
                    "disconnect" => Some(
                        "Turn Wi-Fi off? This may cut the connection you are working over.".into(),
                    ),
                    _ => None,
                }
            }
            "bluetooth_pair" => {
                return matches!(action, "pair" | "unpair" | "connect").then(|| {
                    format!(
                        "{action} the Bluetooth device '{}'? A paired device keeps standing access \
                         to this machine, including, for a keyboard, the ability to type on it.",
                        args.get("device").and_then(Value::as_str).unwrap_or("?")
                    )
                })
            }
            "http_request" => {}
            _ => return None,
        }
        let method = args
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("GET")
            .to_ascii_uppercase();
        let url = args.get("url").and_then(Value::as_str).unwrap_or("?");
        let has_body = args
            .get("body")
            .and_then(Value::as_str)
            .is_some_and(|b| !b.is_empty());
        if method == "GET" || method == "HEAD" {
            has_body.then(|| format!("Send a {method} with a request body to {url}."))
        } else {
            Some(format!(
                "Send a {method} request to {url}, which may change remote state."
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module(hosts: Vec<&str>) -> NetModule {
        NetModule::new(
            NetPolicy {
                allowed_hosts: hosts.into_iter().map(str::to_string).collect(),
                allow_private: false,
            },
            10,
            100_000,
        )
    }

    fn ctx() -> CallCtx {
        CallCtx::new("t", mcp_types::CancelToken::new())
    }

    /// The attack this engine exists to stop.
    #[tokio::test]
    async fn cloud_metadata_and_loopback_are_refused() {
        let m = module(vec!["169.254.169.254", "localhost", "127.0.0.1"]);
        for url in [
            "http://169.254.169.254/latest/meta-data/",
            "http://localhost:8080/admin",
            "http://127.0.0.1/",
        ] {
            let env = m.call("http_request", json!({ "url": url }), &ctx()).await;
            assert!(!env.ok, "{url} should be blocked");
            assert_eq!(
                env.error.unwrap().code,
                ErrorCode::PolicyDenied,
                "{url} should be policy-denied"
            );
        }
    }

    #[tokio::test]
    async fn hosts_outside_the_allowlist_are_refused() {
        let m = module(vec!["example.com"]);
        let env = m
            .call(
                "http_request",
                json!({ "url": "https://evil.example/" }),
                &ctx(),
            )
            .await;
        assert!(!env.ok);
        assert_eq!(env.error.unwrap().code, ErrorCode::PolicyDenied);
    }

    #[tokio::test]
    async fn non_http_schemes_are_refused() {
        let m = module(vec!["example.com"]);
        for url in ["file:///etc/passwd", "gopher://example.com/"] {
            let env = m.call("http_request", json!({ "url": url }), &ctx()).await;
            assert!(!env.ok, "{url} should be refused");
        }
    }

    #[tokio::test]
    async fn with_no_allowlist_nothing_is_reachable() {
        let m = module(vec![]);
        let env = m
            .call(
                "http_request",
                json!({ "url": "https://example.com/" }),
                &ctx(),
            )
            .await;
        assert!(!env.ok);
    }

    /// State-changing requests and GETs carrying a body must reach a human.
    #[test]
    fn state_changing_requests_require_consent() {
        let m = module(vec!["example.com"]);
        assert!(m
            .consent_prompt(
                "http_request",
                &json!({ "url": "https://example.com/", "method": "GET" })
            )
            .is_none());
        let p = m
            .consent_prompt(
                "http_request",
                &json!({ "url": "https://example.com/x", "method": "POST" }),
            )
            .unwrap();
        assert!(p.contains("POST") && p.contains("example.com"), "{p}");
        assert!(m
            .consent_prompt(
                "http_request",
                &json!({ "url": "https://example.com/", "method": "GET", "body": "leak" })
            )
            .is_some());
        assert!(m
            .consent_prompt("dns_lookup", &json!({ "host": "x" }))
            .is_none());
    }

    #[tokio::test]
    async fn dns_lookup_flags_internal_addresses() {
        let m = module(vec![]);
        let env = m
            .call("dns_lookup", json!({ "host": "localhost" }), &ctx())
            .await;
        assert!(env.ok, "{env:?}");
        let d = env.data.unwrap();
        let addrs = d["addresses"].as_array().unwrap();
        assert!(!addrs.is_empty());
        assert!(addrs
            .iter()
            .all(|a| a["private_or_internal"] == json!(true)));
    }

    #[tokio::test]
    async fn interfaces_are_enumerated() {
        let env = module(vec![])
            .call("network_interfaces", json!({}), &ctx())
            .await;
        assert!(env.ok, "{env:?}");
        assert!(!env.data.unwrap()["interfaces"]
            .as_array()
            .unwrap()
            .is_empty());
    }
}

#[cfg(test)]
mod extended_tests {
    use super::*;
    use mcp_types::CancelToken;

    fn ctx() -> CallCtx {
        CallCtx::new("t", CancelToken::new())
    }
    fn module(hosts: Vec<&str>) -> NetModule {
        NetModule::new(
            NetPolicy {
                allowed_hosts: hosts.iter().map(|h| h.to_string()).collect(),
                allow_private: false,
            },
            10,
            100_000,
        )
    }

    #[tokio::test]
    async fn socket_inspection_finds_real_listeners_with_owners() {
        let env = module(vec![])
            .call("socket_inspection", json!({ "proto": "tcp" }), &ctx())
            .await;
        assert!(env.ok, "{env:?}");
        let d = env.data.unwrap();
        let socks = d["sockets"].as_array().unwrap();
        assert!(!socks.is_empty(), "a live machine has open TCP sockets");
        assert!(
            socks.iter().all(|s| s["command"].as_str().is_some()),
            "the owning process is the point of this tool"
        );
    }

    /// A probe is a reach-out. "Just a ping" to a metadata address is still a
    /// packet to the metadata address.
    #[tokio::test]
    async fn probes_obey_the_same_allowlist_as_http() {
        let m = module(vec!["example.com"]);
        for target in ["169.254.169.254", "127.0.0.1", "internal.corp"] {
            let env = m
                .call(
                    "packet_diagnostics",
                    json!({ "op": "ping", "target": target }),
                    &ctx(),
                )
                .await;
            assert!(!env.ok, "{target} must be refused");
            assert_eq!(env.error.unwrap().code, ErrorCode::PolicyDenied, "{target}");
        }
        // And with nothing allowlisted, nothing is reachable.
        let closed = module(vec![]);
        let env = closed
            .call(
                "packet_diagnostics",
                json!({ "op": "ping", "target": "example.com" }),
                &ctx(),
            )
            .await;
        assert!(!env.ok);
    }

    #[tokio::test]
    async fn probe_targets_are_validated() {
        let m = module(vec!["example.com"]);
        for bad in ["-c 1000", "a;b", "a b", ""] {
            let env = m
                .call(
                    "packet_diagnostics",
                    json!({ "op": "ping", "target": bad }),
                    &ctx(),
                )
                .await;
            assert!(!env.ok, "{bad:?} must be refused");
            assert_eq!(env.error.unwrap().code, ErrorCode::InvalidArgs, "{bad:?}");
        }
        let env = m
            .call(
                "packet_diagnostics",
                json!({ "op": "nmap", "target": "example.com" }),
                &ctx(),
            )
            .await;
        assert!(!env.ok, "unknown op must be refused");
    }

    #[tokio::test]
    async fn firewall_list_reads_real_state() {
        let env = module(vec![])
            .call("firewall_rules", json!({ "action": "list" }), &ctx())
            .await;
        assert!(env.ok, "{env:?}");
        assert!(env.data.unwrap()["enabled"].is_boolean());
    }

    #[tokio::test]
    async fn firewall_rules_must_be_absolute_app_paths() {
        let env = module(vec![])
            .call(
                "firewall_rules",
                json!({ "action": "add", "rule": "--setglobalstate" }),
                &ctx(),
            )
            .await;
        assert!(!env.ok);
        assert_eq!(env.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn wifi_and_vpn_state_are_readable() {
        let m = module(vec![]);
        let vpn = m
            .call("network_manage", json!({ "action": "vpn_status" }), &ctx())
            .await;
        assert!(vpn.ok, "{vpn:?}");
        let wifi = m
            .call("network_manage", json!({ "action": "list_wifi" }), &ctx())
            .await;
        // A machine with no Wi-Fi interface is a legitimate NOT_FOUND.
        if !wifi.ok {
            assert_eq!(wifi.error.unwrap().code, ErrorCode::NotFound);
        } else {
            assert!(wifi.data.unwrap()["device"].as_str().is_some());
        }
    }

    /// The secret must not come back in the response: that is how it ends up
    /// in a transcript, an audit line, and a model's context.
    #[tokio::test]
    async fn a_wifi_secret_is_never_echoed_back() {
        let env = module(vec![])
            .call(
                "network_manage",
                json!({ "action": "connect", "ssid": "nonexistent-net-xyzzy",
                        "secret": "hunter2-should-not-appear" }),
                &ctx(),
            )
            .await;
        let text = serde_json::to_string(&env).unwrap();
        assert!(
            !text.contains("hunter2-should-not-appear"),
            "the Wi-Fi password leaked into the response: {text}"
        );
    }

    #[tokio::test]
    async fn bluetooth_lists_without_extra_tools_and_says_what_pairing_needs() {
        let m = module(vec![]);
        let listed = m
            .call("bluetooth_pair", json!({ "action": "list" }), &ctx())
            .await;
        assert!(listed.ok, "{listed:?}");

        let bad = m
            .call(
                "bluetooth_pair",
                json!({ "action": "pair", "device": "notanaddress" }),
                &ctx(),
            )
            .await;
        assert!(!bad.ok);
        assert_eq!(bad.error.unwrap().code, ErrorCode::InvalidArgs);

        let pair = m
            .call(
                "bluetooth_pair",
                json!({ "action": "pair", "device": "aa-bb-cc-dd-ee-ff" }),
                &ctx(),
            )
            .await;
        if !pair.ok {
            let e = pair.error.unwrap();
            if e.code == ErrorCode::UnsupportedOs {
                assert!(
                    e.suggestion.unwrap().contains("blueutil"),
                    "must name the missing tool"
                );
            }
        }
    }

    #[tokio::test]
    async fn only_mutations_ask_for_consent() {
        let m = module(vec![]);
        for (tool, args) in [
            ("firewall_rules", json!({ "action": "list" })),
            ("network_manage", json!({ "action": "list_wifi" })),
            ("network_manage", json!({ "action": "vpn_status" })),
            ("bluetooth_pair", json!({ "action": "list" })),
            ("socket_inspection", json!({})),
        ] {
            assert!(m.consent_prompt(tool, &args).is_none(), "{tool} {args}");
        }
        for (tool, args) in [
            (
                "firewall_rules",
                json!({ "action": "add", "rule": "/Applications/X.app" }),
            ),
            (
                "network_manage",
                json!({ "action": "connect", "ssid": "Cafe" }),
            ),
            (
                "bluetooth_pair",
                json!({ "action": "pair", "device": "aa-bb-cc-dd-ee-ff" }),
            ),
        ] {
            assert!(m.consent_prompt(tool, &args).is_some(), "{tool} must ask");
        }
        // A paired keyboard can type on the machine; the prompt should say so.
        let p = m
            .consent_prompt(
                "bluetooth_pair",
                &json!({ "action": "pair", "device": "aa-bb-cc-dd-ee-ff" }),
            )
            .unwrap();
        assert!(p.contains("keyboard"), "{p}");
    }
}
