//! System / hardware diagnostics (`docs/planning.md` §5.7).
//!
//! Entirely read-only: telemetry, OS identity, disk usage, logs. Built on
//! platform CLIs (`sysctl`, `vm_stat`, `df`, `uptime`, `log`) rather than a
//! `sysinfo` dependency, keeping the build lean.
//!
//! `sys_logs` is the one tool here worth care: system logs routinely contain
//! hostnames, usernames and process arguments, so output is capped and the
//! caller must supply a predicate rather than dumping everything.

use async_trait::async_trait;
use mcp_types::{CallCtx, Category, Envelope, ErrorCode, Tier, ToolDescriptor, ToolModule};
use serde_json::{json, Value};

pub struct SysModule {
    max_log_lines: usize,
}

impl Default for SysModule {
    fn default() -> Self {
        Self::new(200)
    }
}

fn run(program: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).to_string())
}

/// Read one `sysctl` key as a trimmed string.
fn sysctl(key: &str) -> Option<String> {
    run("/usr/sbin/sysctl", &["-n", key]).map(|s| s.trim().to_string())
}

/// Parse `pmset -g batt`, which looks like:
///
/// ```text
/// Now drawing from 'Battery Power'
///  -InternalBattery-0 (id=...)    45%; discharging; 4:28 remaining present: true
/// ```
///
/// The percentage alone does not answer the question an agent actually has —
/// "can this machine finish the job?" — so the source, the charging state and
/// the time estimate are all kept.
fn parse_power(text: &str) -> Option<Value> {
    let mut out = json!({});
    if let Some(src) = text
        .lines()
        .next()
        .and_then(|l| l.split('\'').nth(1))
        .filter(|s| !s.is_empty())
    {
        out["source"] = json!(src);
        out["on_ac"] = json!(src.eq_ignore_ascii_case("AC Power"));
    }
    // The battery line is the one carrying a percentage.
    let line = text.lines().find(|l| l.contains('%'))?;
    // `45%; discharging; 4:28 remaining present: true`
    let fields: Vec<&str> = line.split(';').map(str::trim).collect();
    for (i, f) in fields.iter().enumerate() {
        if i == 0 {
            if let Some(pct) = f
                .rsplit(char::is_whitespace)
                .next()
                .and_then(|t| t.trim_end_matches('%').parse::<u8>().ok())
            {
                out["percent"] = json!(pct);
            }
            continue;
        }
        // The last field trails junk: `4:28 remaining present: true`.
        if let Some(idx) = f.find(" remaining") {
            let est = f[..idx].trim();
            if !est.is_empty() {
                out["time_remaining"] = json!(est);
            }
        } else if !f.contains(':') {
            // Anything else without a `key: value` shape is the charge state
            // (`discharging`, `charging`, `charged`, `AC attached`).
            out["state"] = json!(*f);
        }
    }
    (out.as_object().is_some_and(|m| !m.is_empty())).then_some(out)
}

impl SysModule {
    pub fn new(max_log_lines: usize) -> Self {
        SysModule { max_log_lines }
    }

    fn os_info(&self) -> Envelope {
        let mut data = json!({
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
        });
        if let Some(v) = run("/usr/bin/uname", &["-r"]) {
            data["kernel"] = json!(v.trim());
        }
        if let Some(v) = run("/usr/bin/sw_vers", &["-productVersion"]) {
            data["os_version"] = json!(v.trim());
        }
        if let Some(v) = sysctl("hw.model") {
            data["model"] = json!(v);
        }
        if let Some(v) = run("/bin/hostname", &[]) {
            data["hostname"] = json!(v.trim());
        }
        if let Some(v) = run("/usr/bin/uptime", &[]) {
            data["uptime"] = json!(v.trim());
        }
        Envelope::ok("os_info", data)
    }

    fn telemetry(&self) -> Envelope {
        let mut data = json!({});
        if let Some(n) = sysctl("hw.ncpu").and_then(|v| v.parse::<u64>().ok()) {
            data["cpu_count"] = json!(n);
        }
        if let Some(b) = sysctl("hw.memsize").and_then(|v| v.parse::<u64>().ok()) {
            data["memory_total_bytes"] = json!(b);
        }
        // Load average, from uptime's tail.
        if let Some(u) = run("/usr/bin/uptime", &[]) {
            if let Some(idx) = u.find("load averages:").or_else(|| u.find("load average:")) {
                let tail = &u[idx..];
                let nums: Vec<f64> = tail
                    .split_whitespace()
                    .filter_map(|t| t.trim_end_matches(',').parse::<f64>().ok())
                    .take(3)
                    .collect();
                if !nums.is_empty() {
                    data["load_average"] = json!(nums);
                }
            }
        }
        // Free memory from vm_stat's page counters.
        if let Some(vm) = run("/usr/bin/vm_stat", &[]) {
            let page = vm
                .lines()
                .next()
                .and_then(|l| l.split("page size of ").nth(1))
                .and_then(|t| t.split_whitespace().next())
                .and_then(|n| n.parse::<u64>().ok())
                .unwrap_or(4096);
            let free_pages: u64 = vm
                .lines()
                .find(|l| l.starts_with("Pages free:"))
                .and_then(|l| l.split(':').nth(1))
                .and_then(|v| v.trim().trim_end_matches('.').parse::<u64>().ok())
                .unwrap_or(0);
            if free_pages > 0 {
                data["memory_free_bytes"] = json!(free_pages * page);
            }
        }
        if let Some(bat) = run("/usr/bin/pmset", &["-g", "batt"]) {
            if let Some(p) = parse_power(&bat) {
                data["power"] = p;
            }
        }
        Envelope::ok("hardware_telemetry", data)
    }

    fn disk_usage(&self) -> Envelope {
        let Some(text) = run("/bin/df", &["-k"]) else {
            return Envelope::fail(
                "disk_usage",
                ErrorCode::UnsupportedOs,
                "df is unavailable on this platform",
            );
        };
        let mut vols = Vec::new();
        for line in text.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 6 {
                continue;
            }
            let kb = |i: usize| f[i].parse::<u64>().unwrap_or(0) * 1024;
            vols.push(json!({
                "filesystem": f[0],
                "total_bytes": kb(1),
                "used_bytes": kb(2),
                "available_bytes": kb(3),
                "capacity": f[4],
                "mount": f[f.len() - 1],
            }));
        }
        Envelope::ok("disk_usage", json!({ "volumes": vols }))
    }

    /// Connected devices, by bus.
    fn bus_devices(&self, args: &Value) -> Envelope {
        let tool = "bus_devices";
        let bus = args.get("bus").and_then(Value::as_str).unwrap_or("all");
        let types: Vec<&str> = match bus {
            "usb" => vec!["SPUSBDataType"],
            "pci" => vec!["SPPCIDataType"],
            "bluetooth" => vec!["SPBluetoothDataType"],
            "all" => vec!["SPUSBDataType", "SPPCIDataType", "SPBluetoothDataType"],
            other => {
                return Envelope::fail(
                    tool,
                    ErrorCode::InvalidArgs,
                    format!("unknown bus '{other}' (usb|pci|bluetooth|all)"),
                )
            }
        };
        let mut argv = vec!["-json"];
        argv.extend_from_slice(&types);
        let Some(text) = run("/usr/sbin/system_profiler", &argv) else {
            return Envelope::fail(
                tool,
                ErrorCode::UnsupportedOs,
                "system_profiler is unavailable on this platform",
            );
        };
        match serde_json::from_str::<Value>(&text) {
            Ok(v) => {
                let count = v
                    .as_object()
                    .map(|o| {
                        o.values()
                            .filter_map(Value::as_array)
                            .map(|a| a.len())
                            .sum::<usize>()
                    })
                    .unwrap_or(0);
                Envelope::ok(tool, json!({ "bus": bus, "devices": v, "count": count }))
            }
            Err(e) => Envelope::fail(
                tool,
                ErrorCode::ActionFailed,
                format!("system_profiler returned unparsable JSON: {e}"),
            ),
        }
    }

    /// Read another process's memory map, or its bytes.
    ///
    /// Write-never by design (D9). Raw reads need the OS to grant attach rights,
    /// which on macOS means a signed debugger entitlement or root — neither of
    /// which this server has or acquires — so on an ordinary machine the map is
    /// what you get and a byte read reports `PERM_DENIED` honestly.
    fn proc_memory_read(&self, args: &Value) -> Envelope {
        let tool = "proc_memory_read";
        let Some(pid) = args.get("pid").and_then(Value::as_i64) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'pid'");
        };
        if pid <= 1 {
            return Envelope::fail(
                tool,
                ErrorCode::PolicyDenied,
                "refusing to inspect pid <= 1",
            );
        }
        // Same-user only, unless the OS itself grants more. Reading another
        // user's process memory is credential theft with extra steps.
        let owner = run("/bin/ps", &["-o", "user=", "-p", &pid.to_string()])
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if owner.is_empty() {
            return Envelope::fail(tool, ErrorCode::NotFound, format!("no process {pid}"));
        }
        let me = run("/usr/bin/id", &["-un"])
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if owner != me {
            return Envelope::fail_with(
                tool,
                ErrorCode::PermDenied,
                format!("process {pid} belongs to '{owner}', not '{me}'"),
                "only this user's own processes can be inspected",
            );
        }

        let maps_only = args
            .get("maps_only")
            .and_then(Value::as_bool)
            .unwrap_or_else(|| args.get("address").is_none());
        if maps_only {
            let Some(text) = run("/usr/bin/vmmap", &["--summary", &pid.to_string()]) else {
                return Envelope::fail_with(
                    tool,
                    ErrorCode::PermDenied,
                    format!("could not read the memory map of {pid}"),
                    "macOS restricts inspecting other processes; try a process you started",
                );
            };
            let regions: Vec<Value> = text
                .lines()
                .skip_while(|l| !l.contains("REGION TYPE"))
                .skip(1)
                .filter(|l| !l.trim().is_empty())
                .take(200)
                .map(|l| json!({ "region": l.trim() }))
                .collect();
            return Envelope::ok(
                tool,
                json!({ "pid": pid, "maps": regions, "count": regions.len(), "maps_only": true }),
            );
        }

        let Some(address) = args.get("address").and_then(parse_address) else {
            return Envelope::fail_with(
                tool,
                ErrorCode::InvalidArgs,
                "'address' must be a number or a 0x-prefixed string",
                "or pass maps_only=true to read the region map instead",
            );
        };
        let length = args
            .get("length")
            .and_then(Value::as_u64)
            .unwrap_or(256)
            .clamp(1, 4096);
        match read_process_memory(pid as i32, address, length as usize) {
            Ok(bytes) => Envelope::ok(
                tool,
                json!({
                    "pid": pid, "address": format!("0x{address:x}"),
                    "length": bytes.len(), "hex": to_hex(&bytes),
                }),
            ),
            Err(e) => Envelope::fail_with(
                tool,
                ErrorCode::PermDenied,
                e,
                "macOS grants task ports only to signed debuggers or root; agentctl does not \
                 escalate, so use maps_only=true for what is readable without it",
            ),
        }
    }

    /// Read or write a system configuration value.
    ///
    /// Two stores: `defaults` (per-user preferences) and `sysctl` (kernel
    /// parameters). Writing either changes machine behaviour outside this
    /// session, so writes are gated and the key is validated as argv.
    fn system_config(&self, args: &Value) -> Envelope {
        let tool = "system_config";
        let store = args
            .get("store")
            .and_then(Value::as_str)
            .unwrap_or("defaults");
        let action = args.get("action").and_then(Value::as_str).unwrap_or("read");
        let Some(key) = args.get("key").and_then(Value::as_str) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'key'");
        };
        if !valid_config_key(key) {
            return Envelope::fail_with(
                tool,
                ErrorCode::InvalidArgs,
                format!("'{key}' is not a valid configuration key"),
                "keys are dotted identifiers; a leading '-' would reach the tool as an option",
            );
        }
        match (store, action) {
            ("sysctl", "read") => match run("/usr/sbin/sysctl", &["-n", key]) {
                Some(v) => Envelope::ok(
                    tool,
                    json!({ "store": store, "key": key, "value": v.trim() }),
                ),
                None => Envelope::fail(tool, ErrorCode::NotFound, format!("no sysctl '{key}'")),
            },
            ("sysctl", "write") => {
                let Some(value) = args.get("value").and_then(Value::as_str) else {
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, "'write' needs 'value'");
                };
                if !valid_config_value(value) {
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, "invalid 'value'");
                }
                match run("/usr/sbin/sysctl", &["-w", &format!("{key}={value}")]) {
                    Some(out) => Envelope::ok(
                        tool,
                        json!({ "store": store, "key": key, "written": value, "detail": out.trim() }),
                    ),
                    None => Envelope::fail_with(
                        tool,
                        ErrorCode::PermDenied,
                        format!("could not set sysctl '{key}'"),
                        "most kernel parameters need root; agentctl does not escalate on its own",
                    ),
                }
            }
            ("defaults", "read") => {
                let (domain, name) = split_domain(key);
                let argv: Vec<&str> = match name {
                    Some(n) => vec!["read", domain, n],
                    None => vec!["read", domain],
                };
                match run("/usr/bin/defaults", &argv) {
                    Some(v) => Envelope::ok(
                        tool,
                        json!({ "store": store, "key": key, "value": v.trim() }),
                    ),
                    None => Envelope::fail(
                        tool,
                        ErrorCode::NotFound,
                        format!("no defaults entry '{key}'"),
                    ),
                }
            }
            ("defaults", "write") => {
                let Some(value) = args.get("value").and_then(Value::as_str) else {
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, "'write' needs 'value'");
                };
                if !valid_config_value(value) {
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, "invalid 'value'");
                }
                let (domain, Some(name)) = split_domain(key) else {
                    return Envelope::fail(
                        tool,
                        ErrorCode::InvalidArgs,
                        "a defaults write needs 'domain key', e.g. 'com.apple.dock autohide'",
                    );
                };
                match run("/usr/bin/defaults", &["write", domain, name, value]) {
                    Some(_) => Envelope::ok(
                        tool,
                        json!({ "store": store, "key": key, "written": value }),
                    ),
                    None => Envelope::fail(
                        tool,
                        ErrorCode::ActionFailed,
                        format!("could not write '{key}'"),
                    ),
                }
            }
            (s, a) => Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                format!("unsupported store/action '{s}'/'{a}' (defaults|sysctl x read|write)"),
            ),
        }
    }

    fn logs(&self, args: &Value) -> Envelope {
        let tool = "sys_logs";
        // A predicate is required: system logs carry usernames, hostnames and
        // process arguments, so an unfiltered dump is a needless disclosure.
        let Some(query) = args
            .get("query")
            .and_then(Value::as_str)
            .filter(|q| !q.is_empty())
        else {
            return Envelope::fail_with(
                tool,
                ErrorCode::InvalidArgs,
                "missing 'query'",
                "supply a search term; unfiltered log dumps are not permitted",
            );
        };
        if query.len() > 200 {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "'query' is too long");
        }
        let minutes = args
            .get("last_minutes")
            .and_then(Value::as_u64)
            .unwrap_or(15)
            .clamp(1, 1440);
        let lines = args
            .get("max_lines")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(self.max_log_lines)
            .min(self.max_log_lines);

        // `log show` takes a predicate; pass the term as data via argv, never a
        // shell string, and quote it inside the predicate expression.
        let escaped = query.replace('\\', "\\\\").replace('"', "\\\"");
        let predicate = format!("eventMessage CONTAINS \"{escaped}\"");
        let out = run(
            "/usr/bin/log",
            &[
                "show",
                "--style",
                "compact",
                "--last",
                &format!("{minutes}m"),
                "--predicate",
                &predicate,
            ],
        );
        match out {
            Some(text) => {
                let collected: Vec<&str> = text.lines().take(lines).collect();
                let truncated = text.lines().count() > collected.len();
                Envelope::ok(
                    tool,
                    json!({
                        "query": query, "last_minutes": minutes,
                        "lines": collected, "truncated": truncated,
                    }),
                )
            }
            None => Envelope::fail(
                tool,
                ErrorCode::UnsupportedOs,
                "system log query is unavailable on this platform",
            ),
        }
    }
}

/// A dotted configuration key, optionally `domain key` for `defaults`.
fn valid_config_key(k: &str) -> bool {
    !k.is_empty()
        && k.len() <= 128
        && !k.starts_with('-')
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ' '))
        && k.split_whitespace().count() <= 2
}

fn valid_config_value(v: &str) -> bool {
    v.len() <= 256 && !v.starts_with('-') && !v.contains('\n')
}

fn split_domain(key: &str) -> (&str, Option<&str>) {
    match key.split_once(' ') {
        Some((d, n)) => (d, Some(n.trim())),
        None => (key, None),
    }
}

fn parse_address(v: &Value) -> Option<u64> {
    if let Some(n) = v.as_u64() {
        return Some(n);
    }
    let s = v.as_str()?.trim();
    let s = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
    u64::from_str_radix(s, 16).ok()
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Read `length` bytes at `address` from another process.
///
/// `task_for_pid` is the gate: macOS hands out a task port only to a process
/// with the debugger entitlement or to root. Failure here is the *expected*
/// outcome on a normal machine, and it is reported as a permission problem
/// rather than dressed up as something retryable.
#[cfg(target_os = "macos")]
fn read_process_memory(pid: i32, address: u64, length: usize) -> Result<Vec<u8>, String> {
    extern "C" {
        static mach_task_self_: libc::mach_port_t;
        fn task_for_pid(
            target: libc::mach_port_t,
            pid: libc::c_int,
            task: *mut libc::mach_port_t,
        ) -> libc::c_int;
        fn mach_vm_read_overwrite(
            task: libc::mach_port_t,
            address: u64,
            size: u64,
            data: u64,
            out_size: *mut u64,
        ) -> libc::c_int;
    }
    const KERN_SUCCESS: libc::c_int = 0;
    let mut task: libc::mach_port_t = 0;
    let kr = unsafe { task_for_pid(mach_task_self_, pid, &mut task) };
    if kr != KERN_SUCCESS {
        return Err(format!(
            "task_for_pid({pid}) failed with kern_return {kr}: the OS did not grant attach rights"
        ));
    }
    let mut buf = vec![0u8; length];
    let mut got: u64 = 0;
    let kr = unsafe {
        mach_vm_read_overwrite(
            task,
            address,
            length as u64,
            buf.as_mut_ptr() as u64,
            &mut got,
        )
    };
    if kr != KERN_SUCCESS {
        return Err(format!(
            "reading 0x{address:x} failed with kern_return {kr} (unmapped or protected)"
        ));
    }
    buf.truncate(got as usize);
    Ok(buf)
}

#[cfg(not(target_os = "macos"))]
fn read_process_memory(_pid: i32, _address: u64, _length: usize) -> Result<Vec<u8>, String> {
    Err("raw process-memory reads are implemented for macOS only".into())
}

#[async_trait]
impl ToolModule for SysModule {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        vec![
            ToolDescriptor::new(
                "os_info",
                Category::System,
                Tier::Read,
                "OS, kernel, model, hostname and uptime.",
                json!({ "type": "object", "properties": {}, "required": [] }),
            ),
            ToolDescriptor::new(
                "hardware_telemetry",
                Category::System,
                Tier::Read,
                "CPU count, memory totals, load average and battery.",
                json!({ "type": "object", "properties": {}, "required": [] }),
            ),
            ToolDescriptor::new(
                "disk_usage",
                Category::System,
                Tier::Read,
                "Mounted volumes with total/used/available bytes.",
                json!({ "type": "object", "properties": {}, "required": [] }),
            ),
            ToolDescriptor::new(
                "bus_devices",
                Category::System,
                Tier::Read,
                "Connected USB, PCI and Bluetooth devices.",
                json!({"type":"object","properties":{
                    "bus":{"type":"string","enum":["usb","pci","bluetooth","all"]}},
                    "required":[]}),
            ),
            ToolDescriptor::new(
                "proc_memory_read",
                Category::System,
                Tier::Dangerous,
                "Read another process's memory map, or its bytes. Read-only, this user's own \
                 processes only. Byte reads need attach rights the OS grants to signed debuggers \
                 and root; without them the region map is what is available.",
                json!({"type":"object","properties":{
                    "pid":{"type":"integer"},
                    "address":{"type":"string","description":"decimal or 0x-prefixed"},
                    "length":{"type":"integer","description":"bytes, max 4096"},
                    "maps_only":{"type":"boolean"}},
                    "required":["pid"]}),
            ),
            ToolDescriptor::new(
                "system_config",
                Category::System,
                Tier::Dangerous,
                "Read or write a preference (defaults) or kernel parameter (sysctl). Writes change \
                 machine behaviour beyond this session.",
                json!({"type":"object","properties":{
                    "store":{"type":"string","enum":["defaults","sysctl"]},
                    "action":{"type":"string","enum":["read","write"]},
                    "key":{"type":"string","description":"sysctl name, or 'domain key' for defaults"},
                    "value":{"type":"string"}},
                    "required":["store","action","key"]}),
            ),
            ToolDescriptor::new(
                "sys_logs",
                Category::System,
                Tier::Read,
                "Search recent system logs. A query is required — unfiltered dumps are refused.",
                json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "last_minutes": { "type": "integer" },
                        "max_lines": { "type": "integer" }
                    },
                    "required": ["query"]
                }),
            ),
        ]
    }

    /// Reading another process's bytes and rewriting machine configuration both
    /// reach outside this session; the region map and plain reads do not.
    fn consent_prompt(&self, name: &str, args: &Value) -> Option<String> {
        match name {
            "proc_memory_read" => {
                let maps_only = args
                    .get("maps_only")
                    .and_then(Value::as_bool)
                    .unwrap_or_else(|| args.get("address").is_none());
                (!maps_only).then(|| {
                    format!(
                        "Read raw memory from process {}? Process memory routinely holds \
                         decrypted secrets.",
                        args.get("pid").and_then(Value::as_i64).unwrap_or(0)
                    )
                })
            }
            "system_config" => {
                (args.get("action").and_then(Value::as_str) == Some("write")).then(|| {
                    format!(
                        "Set {} '{}' to '{}'? This changes the machine, not just this session.",
                        args.get("store").and_then(Value::as_str).unwrap_or("?"),
                        args.get("key").and_then(Value::as_str).unwrap_or("?"),
                        args.get("value").and_then(Value::as_str).unwrap_or("?")
                    )
                })
            }
            _ => None,
        }
    }

    async fn call(&self, name: &str, args: Value, _ctx: &CallCtx) -> Envelope {
        match name {
            "os_info" => self.os_info(),
            "hardware_telemetry" => self.telemetry(),
            "disk_usage" => self.disk_usage(),
            "sys_logs" => self.logs(&args),
            "bus_devices" => self.bus_devices(&args),
            "proc_memory_read" => self.proc_memory_read(&args),
            "system_config" => self.system_config(&args),
            other => Envelope::fail(other, ErrorCode::InvalidArgs, "unknown tool"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> CallCtx {
        CallCtx::new("t", mcp_types::CancelToken::new())
    }

    #[tokio::test]
    async fn os_info_reports_the_real_platform() {
        let env = SysModule::default()
            .call("os_info", json!({}), &ctx())
            .await;
        assert!(env.ok, "{env:?}");
        let d = env.data.unwrap();
        assert_eq!(d["os"], std::env::consts::OS);
        assert!(d.get("hostname").is_some());
    }

    #[tokio::test]
    async fn telemetry_reports_plausible_hardware() {
        let env = SysModule::default()
            .call("hardware_telemetry", json!({}), &ctx())
            .await;
        assert!(env.ok, "{env:?}");
        let d = env.data.unwrap();
        assert!(d["cpu_count"].as_u64().unwrap_or(0) >= 1);
        assert!(d["memory_total_bytes"].as_u64().unwrap_or(0) > 0);
    }

    /// Real `pmset -g batt` output on battery. The percentage alone was all the
    /// first version kept; source and time-remaining are what actually tell an
    /// agent whether a long job will survive.
    #[test]
    fn power_parses_a_discharging_laptop() {
        let out = parse_power(
            "Now drawing from 'Battery Power'\n -InternalBattery-0 (id=34865251)\t45%; discharging; 4:28 remaining present: true\n",
        )
        .expect("parsed");
        assert_eq!(out["source"], "Battery Power");
        assert_eq!(out["on_ac"], false);
        assert_eq!(out["percent"], 45);
        assert_eq!(out["state"], "discharging");
        assert_eq!(out["time_remaining"], "4:28");
    }

    #[test]
    fn power_parses_a_charged_machine_on_mains() {
        let out = parse_power(
            "Now drawing from 'AC Power'\n -InternalBattery-0 (id=1)\t100%; charged; 0:00 remaining present: true\n",
        )
        .expect("parsed");
        assert_eq!(out["on_ac"], true);
        assert_eq!(out["percent"], 100);
        assert_eq!(out["state"], "charged");
    }

    /// Desktops have no battery line at all — report the source, invent nothing.
    #[test]
    fn power_without_a_battery_yields_no_percentage() {
        let out = parse_power("Now drawing from 'AC Power'\n");
        assert!(out.is_none() || out.unwrap().get("percent").is_none());
    }

    #[tokio::test]
    async fn telemetry_power_matches_pmset_when_present() {
        let env = SysModule::default()
            .call("hardware_telemetry", json!({}), &ctx())
            .await;
        let d = env.data.unwrap();
        if let Some(p) = d.get("power") {
            assert!(p.get("source").is_some(), "power must name its source: {p}");
        }
    }

    #[tokio::test]
    async fn disk_usage_lists_real_volumes() {
        let env = SysModule::default()
            .call("disk_usage", json!({}), &ctx())
            .await;
        assert!(env.ok, "{env:?}");
        let vols = env.data.unwrap();
        let vols = vols["volumes"].as_array().unwrap();
        assert!(!vols.is_empty());
        assert!(vols
            .iter()
            .any(|v| v["total_bytes"].as_u64().unwrap_or(0) > 0));
    }

    /// An unfiltered log dump would disclose far more than the agent asked for.
    #[tokio::test]
    async fn logs_require_a_query() {
        let m = SysModule::default();
        let env = m.call("sys_logs", json!({}), &ctx()).await;
        assert!(!env.ok);
        let e = env.error.unwrap();
        assert_eq!(e.code, ErrorCode::InvalidArgs);
        assert!(e.suggestion.is_some());

        let env = m.call("sys_logs", json!({ "query": "" }), &ctx()).await;
        assert!(!env.ok, "empty query must also be refused");
    }

    #[tokio::test]
    async fn log_output_is_capped() {
        let m = SysModule::new(5);
        let env = m
            .call(
                "sys_logs",
                json!({ "query": "a", "max_lines": 1000 }),
                &ctx(),
            )
            .await;
        if env.ok {
            let d = env.data.unwrap();
            assert!(d["lines"].as_array().unwrap().len() <= 5, "cap must hold");
        }
    }

    #[tokio::test]
    async fn read_only_tools_never_ask_and_the_sharp_ones_always_do() {
        let m = SysModule::default();
        for t in [
            "os_info",
            "hardware_telemetry",
            "disk_usage",
            "sys_logs",
            "bus_devices",
        ] {
            assert!(
                m.consent_prompt(t, &json!({})).is_none(),
                "{t} is read-only"
            );
        }
        // The region map is a read; pulling bytes out of another process is not.
        assert!(m
            .consent_prompt(
                "proc_memory_read",
                &json!({ "pid": 123, "maps_only": true })
            )
            .is_none());
        assert!(m
            .consent_prompt(
                "proc_memory_read",
                &json!({ "pid": 123, "address": "0x1000" })
            )
            .is_some());
        assert!(m
            .consent_prompt(
                "system_config",
                &json!({ "action": "read", "key": "kern.osrelease" })
            )
            .is_none());
        let w = m
            .consent_prompt(
                "system_config",
                &json!({ "store": "sysctl", "action": "write", "key": "kern.x", "value": "1" }),
            )
            .unwrap();
        assert!(w.contains("not just this session"), "{w}");
    }

    #[tokio::test]
    async fn bus_devices_enumerates_real_hardware() {
        let env = SysModule::default()
            .call("bus_devices", json!({ "bus": "usb" }), &ctx())
            .await;
        assert!(env.ok, "{env:?}");
        assert!(env.data.unwrap()["devices"].is_object());
    }

    /// Reading another user's process memory is credential theft with extra
    /// steps, and pid <= 1 is never a legitimate target.
    #[tokio::test]
    async fn process_memory_is_restricted_to_this_user() {
        let m = SysModule::default();
        let env = m
            .call("proc_memory_read", json!({ "pid": 1 }), &ctx())
            .await;
        assert!(!env.ok);
        assert_eq!(env.error.unwrap().code, ErrorCode::PolicyDenied);

        let env = m
            .call("proc_memory_read", json!({ "pid": 999_999 }), &ctx())
            .await;
        assert!(!env.ok);
        assert_eq!(env.error.unwrap().code, ErrorCode::NotFound);
    }

    /// Our own map must be readable; that is the part that works without a
    /// debugger entitlement.
    #[tokio::test]
    async fn own_process_map_is_readable() {
        let env = SysModule::default()
            .call(
                "proc_memory_read",
                json!({ "pid": std::process::id(), "maps_only": true }),
                &ctx(),
            )
            .await;
        assert!(env.ok, "{env:?}");
        assert_eq!(env.data.unwrap()["maps_only"], true);
    }

    /// A byte read either succeeds or reports a permission problem — never a
    /// vague failure the agent might retry forever.
    #[tokio::test]
    async fn a_byte_read_without_rights_is_perm_denied() {
        let env = SysModule::default()
            .call(
                "proc_memory_read",
                json!({ "pid": std::process::id(), "address": "0x100000000", "length": 16 }),
                &ctx(),
            )
            .await;
        if !env.ok {
            let e = env.error.unwrap();
            assert_eq!(e.code, ErrorCode::PermDenied);
            assert!(e.suggestion.unwrap().contains("maps_only"));
        }
    }

    #[tokio::test]
    async fn config_keys_are_validated_and_reads_work() {
        let m = SysModule::default();
        let ok = m
            .call(
                "system_config",
                json!({ "store": "sysctl", "action": "read", "key": "kern.osrelease" }),
                &ctx(),
            )
            .await;
        assert!(ok.ok, "{ok:?}");
        assert!(!ok.data.unwrap()["value"].as_str().unwrap().is_empty());

        for bad in ["-w", "a;b", "a b c d"] {
            let env = m
                .call(
                    "system_config",
                    json!({ "store": "sysctl", "action": "read", "key": bad }),
                    &ctx(),
                )
                .await;
            assert!(!env.ok, "{bad:?} must be refused");
            assert_eq!(env.error.unwrap().code, ErrorCode::InvalidArgs);
        }
    }
}
