//! Linux backends for the system engine.
//!
//! Everything here reads the kernel's own interfaces (`/proc`, `/sys`) rather
//! than shelling out, with two exceptions that have no file-backed equivalent:
//! `journalctl` for logs and `gsettings` for per-user preferences. The result
//! shapes match the macOS path field for field, so a caller never has to
//! branch on the platform.

use std::io::Read;
use std::path::{Path, PathBuf};

use mcp_types::{Envelope, ErrorCode};
use serde_json::{json, Value};

use crate::{
    parse_address, run, split_domain, to_hex, valid_config_key, valid_config_value, SysModule,
};

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

// ---- telemetry -------------------------------------------------------------

/// `MemTotal:       28495744 kB` and friends, in bytes.
pub(crate) fn parse_meminfo(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        let (k, rest) = l.split_once(':')?;
        if k.trim() != key {
            return None;
        }
        let mut it = rest.split_whitespace();
        let n: u64 = it.next()?.parse().ok()?;
        Some(match it.next() {
            Some("kB") => n.checked_mul(1024)?,
            Some("mB") => n.checked_mul(1024 * 1024)?,
            _ => n,
        })
    })
}

/// The three load figures at the head of `/proc/loadavg`.
pub(crate) fn parse_loadavg(text: &str) -> Option<Vec<f64>> {
    let nums: Vec<f64> = text
        .split_whitespace()
        .take(3)
        .filter_map(|t| t.parse::<f64>().ok())
        .collect();
    (nums.len() == 3).then_some(nums)
}

/// Logical CPUs, counted as `processor` stanzas in `/proc/cpuinfo`.
pub(crate) fn parse_cpu_count(text: &str) -> usize {
    text.lines()
        .filter(|l| {
            l.split_once(':')
                .is_some_and(|(k, _)| k.trim() == "processor")
        })
        .count()
}

pub(crate) fn parse_cpu_model(text: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        matches!(k.trim(), "model name" | "Model" | "cpu model").then(|| v.trim().to_string())
    })
}

fn read_trimmed(p: &Path) -> Option<String> {
    std::fs::read_to_string(p)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Battery and mains state from a `power_supply` class directory, in the same
/// shape `pmset` yields on macOS: `source`, `on_ac`, `percent`, `state`,
/// `time_remaining`. A machine with no supplies at all (a desktop) yields
/// `None`; nothing is invented.
pub(crate) fn parse_power_supply(dir: &Path) -> Option<Value> {
    let rd = std::fs::read_dir(dir).ok()?;
    let mut mains_online: Option<bool> = None;
    let mut battery: Option<Value> = None;
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        let kind = read_trimmed(&p.join("type")).unwrap_or_default();
        match kind.as_str() {
            "Mains" | "USB" | "Wireless" => {
                let online = read_trimmed(&p.join("online")).as_deref() == Some("1");
                mains_online = Some(mains_online.unwrap_or(false) || online);
            }
            "Battery" if battery.is_none() => {
                // Some laptops expose a peripheral battery (a mouse) under the
                // same class; the one with a capacity figure is the system's.
                let Some(pct) =
                    read_trimmed(&p.join("capacity")).and_then(|v| v.parse::<u8>().ok())
                else {
                    continue;
                };
                let mut b = json!({ "percent": pct.min(100) });
                if let Some(status) = read_trimmed(&p.join("status")) {
                    b["state"] = json!(match status.as_str() {
                        "Full" => "charged".to_string(),
                        other => other.to_ascii_lowercase(),
                    });
                }
                // Seconds from the driver, else derived from energy and draw.
                let secs = read_trimmed(&p.join("time_to_empty_now"))
                    .and_then(|v| v.parse::<u64>().ok())
                    .filter(|s| *s > 0)
                    .or_else(|| {
                        let now = read_trimmed(&p.join("energy_now"))
                            .or_else(|| read_trimmed(&p.join("charge_now")))?
                            .parse::<u64>()
                            .ok()?;
                        let rate = read_trimmed(&p.join("power_now"))
                            .or_else(|| read_trimmed(&p.join("current_now")))?
                            .parse::<u64>()
                            .ok()?;
                        (rate > 0).then(|| now * 3600 / rate)
                    });
                if let Some(s) = secs {
                    b["time_remaining"] = json!(format!("{}:{:02}", s / 3600, (s % 3600) / 60));
                }
                battery = Some(b);
            }
            _ => {}
        }
    }
    let mut out = json!({});
    let on_ac = match (mains_online, &battery) {
        (Some(ac), _) => Some(ac),
        (None, Some(b)) => Some(b["state"] != "discharging"),
        (None, None) => None,
    };
    if let Some(ac) = on_ac {
        out["source"] = json!(if ac { "AC Power" } else { "Battery Power" });
        out["on_ac"] = json!(ac);
    }
    if let Some(b) = battery {
        for (k, v) in b.as_object().into_iter().flatten() {
            out[k] = v.clone();
        }
    }
    (out.as_object().is_some_and(|m| !m.is_empty())).then_some(out)
}

/// Thermal zones as `{type, celsius}`; the kernel reports millidegrees.
pub(crate) fn parse_thermal(dir: &Path) -> Vec<Value> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut zones: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("thermal_zone"))
        })
        .collect();
    zones.sort();
    zones
        .into_iter()
        .filter_map(|z| {
            let milli: i64 = read_trimmed(&z.join("temp"))?.parse().ok()?;
            Some(json!({
                "zone": z.file_name().map(|n| n.to_string_lossy().to_string()),
                "type": read_trimmed(&z.join("type")),
                "celsius": milli as f64 / 1000.0,
            }))
        })
        .collect()
}

// ---- bus devices -----------------------------------------------------------

/// USB devices: every entry under `bus/usb/devices` that carries an
/// `idVendor` is a device (the rest are interfaces and hubs' ports).
pub(crate) fn sysfs_usb(root: &Path) -> Vec<Value> {
    let Ok(rd) = std::fs::read_dir(root.join("bus/usb/devices")) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    dirs.sort();
    dirs.into_iter()
        .filter_map(|d| {
            let vendor = read_trimmed(&d.join("idVendor"))?;
            let mut v = json!({
                "id": d.file_name().map(|n| n.to_string_lossy().to_string()),
                "vendor_id": vendor,
                "product_id": read_trimmed(&d.join("idProduct")),
            });
            for (key, file) in [
                ("product", "product"),
                ("manufacturer", "manufacturer"),
                ("class", "bDeviceClass"),
                ("speed", "speed"),
                ("bus", "busnum"),
                ("device_number", "devnum"),
            ] {
                if let Some(s) = read_trimmed(&d.join(file)) {
                    v[key] = json!(s);
                }
            }
            Some(v)
        })
        .collect()
}

/// PCI devices: vendor, device and class ids as the kernel prints them, plus
/// the bound driver when there is one.
pub(crate) fn sysfs_pci(root: &Path) -> Vec<Value> {
    let Ok(rd) = std::fs::read_dir(root.join("bus/pci/devices")) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    dirs.sort();
    dirs.into_iter()
        .filter_map(|d| {
            let vendor = read_trimmed(&d.join("vendor"))?;
            let mut v = json!({
                "id": d.file_name().map(|n| n.to_string_lossy().to_string()),
                "vendor_id": vendor,
                "device_id": read_trimmed(&d.join("device")),
                "class": read_trimmed(&d.join("class")),
            });
            if let Ok(t) = std::fs::read_link(d.join("driver")) {
                if let Some(n) = t.file_name() {
                    v["driver"] = json!(n.to_string_lossy());
                }
            }
            for (key, file) in [
                ("subsystem_vendor_id", "subsystem_vendor"),
                ("subsystem_device_id", "subsystem_device"),
                ("revision", "revision"),
            ] {
                if let Some(s) = read_trimmed(&d.join(file)) {
                    v[key] = json!(s);
                }
            }
            Some(v)
        })
        .collect()
}

/// Bluetooth: adapters (`hciN`) and the remote devices the kernel currently
/// knows about (`hciN:M`), by address and name.
pub(crate) fn sysfs_bluetooth(root: &Path) -> Vec<Value> {
    let Ok(rd) = std::fs::read_dir(root.join("class/bluetooth")) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    dirs.sort();
    dirs.into_iter()
        .filter_map(|d| {
            let id = d.file_name()?.to_string_lossy().to_string();
            let mut v = json!({
                "id": id,
                "kind": if id.contains(':') { "device" } else { "adapter" },
            });
            for key in ["address", "name", "type"] {
                if let Some(s) = read_trimmed(&d.join(key)) {
                    v[key] = json!(s);
                }
            }
            Some(v)
        })
        .collect()
}

// ---- process memory --------------------------------------------------------

/// The real uid on the `Uid:` line of a `/proc/<pid>/status` file.
pub(crate) fn parse_status_uid(text: &str) -> Option<u32> {
    text.lines().find_map(|l| {
        let rest = l.strip_prefix("Uid:")?;
        rest.split_whitespace().next()?.parse().ok()
    })
}

/// One `/proc/<pid>/maps` line:
/// `55a21d305000-55a21d30c000 r-xp 00000000 00:23 13117826   /usr/bin/head`.
pub(crate) fn parse_maps_line(line: &str) -> Option<Value> {
    let mut f = line.split_whitespace();
    let range = f.next()?;
    let (start, end) = range.split_once('-')?;
    let start = u64::from_str_radix(start, 16).ok()?;
    let end = u64::from_str_radix(end, 16).ok()?;
    let perms = f.next()?;
    let offset = f.next().and_then(|o| u64::from_str_radix(o, 16).ok())?;
    let _dev = f.next()?;
    let _inode = f.next()?;
    let path: String = f.collect::<Vec<_>>().join(" ");
    Some(json!({
        "region": line.trim(),
        "start": format!("0x{start:x}"),
        "end": format!("0x{end:x}"),
        "size": end.saturating_sub(start),
        "perms": perms,
        "offset": offset,
        "path": if path.is_empty() { Value::Null } else { json!(path) },
    }))
}

/// Read `length` bytes at `address` from `/proc/<pid>/mem`.
///
/// The kernel applies the same access check `ptrace(PTRACE_ATTACH)` would, so
/// Yama's `ptrace_scope` and a differing uid both surface here as EACCES or
/// EPERM. Our own process is always readable; an unmapped address is EIO.
pub(crate) fn read_process_memory(
    pid: i32,
    address: u64,
    length: usize,
) -> Result<Vec<u8>, String> {
    use std::os::unix::fs::FileExt;
    let f = std::fs::File::open(format!("/proc/{pid}/mem")).map_err(|e| {
        format!("could not open the memory of {pid}: {e} (the OS did not grant attach rights)")
    })?;
    let mut buf = vec![0u8; length];
    let mut got = 0usize;
    while got < length {
        match f.read_at(&mut buf[got..], address + got as u64) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if got > 0 => {
                tracing::debug!("short read at 0x{:x}: {e}", address + got as u64);
                break;
            }
            Err(e) => {
                return Err(match e.raw_os_error() {
                    Some(5) => format!("reading 0x{address:x} failed: unmapped or protected"),
                    _ => format!("reading 0x{address:x} failed: {e}"),
                })
            }
        }
    }
    buf.truncate(got);
    Ok(buf)
}

// ---- system configuration --------------------------------------------------

/// The macOS spellings an agent is likely to reach for, mapped onto the Linux
/// sysctl with the same meaning. Anything else is taken literally.
pub(crate) fn linux_sysctl_alias(key: &str) -> &str {
    match key {
        "kern.osrelease" => "kernel.osrelease",
        "kern.ostype" => "kernel.ostype",
        "kern.hostname" => "kernel.hostname",
        "kern.version" => "kernel.version",
        "kern.maxfiles" => "fs.file-max",
        "kern.maxproc" => "kernel.pid_max",
        "kern.osrevision" => "kernel.osrelease",
        other => other,
    }
}

/// `kernel.osrelease` to `/proc/sys/kernel/osrelease`. The key has already
/// passed [`valid_config_key`], which forbids `/`; this additionally refuses
/// empty and dot-only components so the path can never climb.
pub(crate) fn sysctl_path(key: &str) -> Option<PathBuf> {
    let key = linux_sysctl_alias(key);
    let mut p = PathBuf::from("/proc/sys");
    for part in key.split('.') {
        if part.is_empty() || part.chars().all(|c| c == '.') || part.contains('/') {
            return None;
        }
        p.push(part);
    }
    Some(p)
}

/// Turn a literal search term into a PCRE that matches it verbatim, for
/// `journalctl --grep`. Every non-alphanumeric byte is escaped, which PCRE
/// accepts for any character.
pub(crate) fn regex_escape(q: &str) -> String {
    let mut out = String::with_capacity(q.len() * 2);
    for c in q.chars() {
        if !c.is_ascii_alphanumeric() && c != ' ' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

impl SysModule {
    /// Extra identity fields the Linux platform can answer: the distribution
    /// name from `os-release` and the hardware model from DMI.
    pub(crate) fn linux_os_info(&self, data: &mut Value) {
        if let Ok(rel) = std::fs::read_to_string("/etc/os-release") {
            if let Some(v) = rel.lines().find_map(|l| l.strip_prefix("PRETTY_NAME=")) {
                data["os_version"] = json!(v.trim().trim_matches('"'));
            }
        }
        let model = ["/sys/class/dmi/id/product_name", "/proc/device-tree/model"]
            .iter()
            .find_map(|p| read_trimmed(Path::new(p)))
            .map(|m| m.trim_end_matches('\0').to_string());
        if let Some(m) = model {
            data["model"] = json!(m);
        }
    }

    pub(crate) fn linux_telemetry(&self) -> Envelope {
        let mut data = json!({});
        let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
        let count = match parse_cpu_count(&cpuinfo) {
            0 => std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(0),
            n => n,
        };
        if count > 0 {
            data["cpu_count"] = json!(count);
        }
        if let Some(m) = parse_cpu_model(&cpuinfo) {
            data["cpu_model"] = json!(m);
        }
        if let Ok(mem) = std::fs::read_to_string("/proc/meminfo") {
            if let Some(t) = parse_meminfo(&mem, "MemTotal") {
                data["memory_total_bytes"] = json!(t);
            }
            // "Available" is what a new allocation can actually get; "Free"
            // undercounts by everything the page cache would give back.
            if let Some(f) =
                parse_meminfo(&mem, "MemAvailable").or_else(|| parse_meminfo(&mem, "MemFree"))
            {
                data["memory_free_bytes"] = json!(f);
            }
        }
        if let Some(l) = std::fs::read_to_string("/proc/loadavg")
            .ok()
            .and_then(|t| parse_loadavg(&t))
        {
            data["load_average"] = json!(l);
        }
        if let Some(p) = parse_power_supply(Path::new("/sys/class/power_supply")) {
            data["power"] = p;
        }
        let zones = parse_thermal(Path::new("/sys/class/thermal"));
        if !zones.is_empty() {
            data["thermal"] = json!(zones);
        }
        Envelope::ok("hardware_telemetry", data)
    }

    pub(crate) fn linux_bus_devices(&self, args: &Value) -> Envelope {
        let tool = "bus_devices";
        let bus = args.get("bus").and_then(Value::as_str).unwrap_or("all");
        let sys = Path::new("/sys");
        let mut devices = json!({});
        match bus {
            "usb" => devices["usb"] = json!(sysfs_usb(sys)),
            "pci" => devices["pci"] = json!(sysfs_pci(sys)),
            "bluetooth" => devices["bluetooth"] = json!(sysfs_bluetooth(sys)),
            "all" => {
                devices["usb"] = json!(sysfs_usb(sys));
                devices["pci"] = json!(sysfs_pci(sys));
                devices["bluetooth"] = json!(sysfs_bluetooth(sys));
            }
            other => {
                return Envelope::fail(
                    tool,
                    ErrorCode::InvalidArgs,
                    format!("unknown bus '{other}' (usb|pci|bluetooth|all)"),
                )
            }
        }
        if !sys.join("bus").is_dir() {
            return Envelope::fail(
                tool,
                ErrorCode::UnsupportedOs,
                "/sys is not mounted, so devices cannot be enumerated",
            );
        }
        let count = devices
            .as_object()
            .map(|o| {
                o.values()
                    .filter_map(Value::as_array)
                    .map(|a| a.len())
                    .sum::<usize>()
            })
            .unwrap_or(0);
        Envelope::ok(
            tool,
            json!({ "bus": bus, "devices": devices, "count": count }),
        )
    }

    /// Same contract as the macOS path: read-only, this user's processes only,
    /// the map is always available for them and a byte read reports
    /// `PERM_DENIED` when the kernel refuses.
    pub(crate) fn linux_proc_memory_read(&self, args: &Value) -> Envelope {
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
        let Some(owner) = std::fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()
            .and_then(|s| parse_status_uid(&s))
        else {
            return Envelope::fail(tool, ErrorCode::NotFound, format!("no process {pid}"));
        };
        let me = std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| parse_status_uid(&s))
            .unwrap_or(u32::MAX);
        if owner != me {
            return Envelope::fail_with(
                tool,
                ErrorCode::PermDenied,
                format!("process {pid} belongs to uid {owner}, not uid {me}"),
                "only this user's own processes can be inspected",
            );
        }

        let maps_only = args
            .get("maps_only")
            .and_then(Value::as_bool)
            .unwrap_or_else(|| args.get("address").is_none());
        if maps_only {
            let text =
                match std::fs::read_to_string(format!("/proc/{pid}/maps")) {
                    Ok(t) => t,
                    Err(e) => return Envelope::fail_with(
                        tool,
                        ErrorCode::PermDenied,
                        format!("could not read the memory map of {pid}: {e}"),
                        "Yama ptrace_scope restricts inspecting other processes; try a process \
                         you started",
                    ),
                };
            let total = text.lines().count();
            let regions: Vec<Value> = text.lines().filter_map(parse_maps_line).take(200).collect();
            return Envelope::ok(
                tool,
                json!({
                    "pid": pid, "maps": regions, "count": regions.len(),
                    "total_regions": total, "maps_only": true,
                }),
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
                "the kernel grants /proc/<pid>/mem only within Yama's ptrace_scope; agentctl does \
                 not escalate, so use maps_only=true for what is readable without it",
            ),
        }
    }

    /// `sysctl` maps onto `/proc/sys`; `defaults` (per-user preferences) maps
    /// onto `gsettings`, which is the same 'domain key' shape.
    pub(crate) fn linux_system_config(&self, args: &Value) -> Envelope {
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
            ("sysctl", "read") => {
                let Some(path) = sysctl_path(key) else {
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, "malformed sysctl key");
                };
                match std::fs::read_to_string(&path) {
                    Ok(v) => Envelope::ok(
                        tool,
                        json!({ "store": store, "key": key, "value": v.trim() }),
                    ),
                    Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Envelope::fail(
                        tool,
                        ErrorCode::PermDenied,
                        format!("sysctl '{key}' is not readable by this user"),
                    ),
                    Err(_) => Envelope::fail_with(
                        tool,
                        ErrorCode::NotFound,
                        format!("no sysctl '{key}'"),
                        "on Linux keys are the kernel's, such as kernel.osrelease (see /proc/sys)",
                    ),
                }
            }
            ("sysctl", "write") => {
                let Some(value) = args.get("value").and_then(Value::as_str) else {
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, "'write' needs 'value'");
                };
                if !valid_config_value(value) {
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, "invalid 'value'");
                }
                let Some(path) = sysctl_path(key) else {
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, "malformed sysctl key");
                };
                if !path.exists() {
                    return Envelope::fail(tool, ErrorCode::NotFound, format!("no sysctl '{key}'"));
                }
                match std::fs::write(&path, format!("{value}\n")) {
                    Ok(()) => Envelope::ok(
                        tool,
                        json!({ "store": store, "key": key, "written": value }),
                    ),
                    Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                        Envelope::fail_with(
                            tool,
                            ErrorCode::PermDenied,
                            format!("could not set sysctl '{key}'"),
                            "most kernel parameters need root; agentctl does not escalate on its own",
                        )
                    }
                    Err(e) => Envelope::fail(
                        tool,
                        ErrorCode::ActionFailed,
                        format!("could not set sysctl '{key}': {e}"),
                    ),
                }
            }
            ("defaults", "read") => {
                let gsettings = match linux_tool("gsettings") {
                    Ok(p) => p,
                    Err(e) => return Envelope::fail(tool, ErrorCode::ActionFailed, e),
                };
                let (domain, name) = split_domain(key);
                let argv: Vec<&str> = match name {
                    Some(n) => vec!["get", domain, n],
                    None => vec!["list-recursively", domain],
                };
                match run(&gsettings, &argv) {
                    Some(v) => Envelope::ok(
                        tool,
                        json!({ "store": store, "key": key, "value": v.trim() }),
                    ),
                    None => Envelope::fail(
                        tool,
                        ErrorCode::NotFound,
                        format!("no gsettings entry '{key}'"),
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
                        "a defaults write needs 'schema key', e.g. 'org.gnome.desktop.interface \
                         color-scheme'",
                    );
                };
                let gsettings = match linux_tool("gsettings") {
                    Ok(p) => p,
                    Err(e) => return Envelope::fail(tool, ErrorCode::ActionFailed, e),
                };
                match run(&gsettings, &["set", domain, name, value]) {
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

    /// Recent journal lines matching a literal term. The term goes to
    /// `journalctl` as an escaped regex in argv, never through a shell.
    pub(crate) fn linux_logs(&self, query: &str, minutes: u64, lines: usize) -> Envelope {
        let tool = "sys_logs";
        let journalctl = match linux_tool("journalctl") {
            Ok(p) => p,
            Err(e) => {
                return Envelope::fail(
                    tool,
                    ErrorCode::UnsupportedOs,
                    format!("system log query is unavailable: {e}"),
                )
            }
        };
        let since = format!("-{minutes}min");
        // One more than the cap, so truncation can be reported honestly.
        let take = format!("{}", lines.saturating_add(1));
        let pattern = regex_escape(query);
        let out = std::process::Command::new(&journalctl)
            .args([
                "--no-pager",
                "-q",
                "-o",
                "short",
                "--since",
                &since,
                "-n",
                &take,
                "--grep",
                &pattern,
            ])
            .stdin(std::process::Stdio::null())
            .output();
        match out {
            Ok(o) if o.status.success() => {
                let text = String::from_utf8_lossy(&o.stdout);
                let all: Vec<&str> = text.lines().collect();
                let truncated = all.len() > lines;
                let collected: Vec<&str> = all.into_iter().take(lines).collect();
                Envelope::ok(
                    tool,
                    json!({
                        "query": query, "last_minutes": minutes,
                        "lines": collected, "truncated": truncated,
                    }),
                )
            }
            Ok(o) => {
                let mut err = String::new();
                let _ = std::io::Cursor::new(o.stderr).read_to_string(&mut err);
                // journalctl exits 1 for "no entries", which is a result.
                if err.trim().is_empty() {
                    return Envelope::ok(
                        tool,
                        json!({
                            "query": query, "last_minutes": minutes,
                            "lines": Vec::<&str>::new(), "truncated": false,
                        }),
                    );
                }
                Envelope::fail(tool, ErrorCode::ActionFailed, err.trim().to_string())
            }
            Err(e) => Envelope::fail(tool, ErrorCode::ActionFailed, format!("journalctl: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mcp-sys-linux-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn put(dir: &Path, name: &str, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), body).unwrap();
    }

    const MEMINFO: &str = "MemTotal:       28495744 kB\nMemFree:         1615768 kB\nMemAvailable:   12966160 kB\nBuffers:            1234 kB\n";

    #[test]
    fn meminfo_scales_kilobytes_to_bytes() {
        assert_eq!(parse_meminfo(MEMINFO, "MemTotal"), Some(28_495_744 * 1024));
        assert_eq!(
            parse_meminfo(MEMINFO, "MemAvailable"),
            Some(12_966_160 * 1024)
        );
        assert_eq!(parse_meminfo(MEMINFO, "Nope"), None);
    }

    #[test]
    fn meminfo_tolerates_garbage_and_emptiness() {
        assert_eq!(parse_meminfo("", "MemTotal"), None);
        assert_eq!(parse_meminfo("MemTotal: lots kB\n", "MemTotal"), None);
        assert_eq!(parse_meminfo("MemTotal:\n", "MemTotal"), None);
        // No unit means bytes as given; an unknown unit is passed through too.
        assert_eq!(
            parse_meminfo("HugePages_Total:       0\n", "HugePages_Total"),
            Some(0)
        );
        // A prefix must not match a longer key.
        assert_eq!(parse_meminfo("MemTotalX: 5 kB\n", "MemTotal"), None);
    }

    #[test]
    fn loadavg_takes_exactly_three_figures() {
        assert_eq!(
            parse_loadavg("2.77 4.18 3.19 13/2300 3230373\n"),
            Some(vec![2.77, 4.18, 3.19])
        );
        assert_eq!(parse_loadavg(""), None);
        assert_eq!(parse_loadavg("1.0 2.0"), None);
        assert_eq!(parse_loadavg("a b c"), None);
    }

    #[test]
    fn cpuinfo_counts_processor_stanzas() {
        let text = "processor\t: 0\nmodel name\t: AMD Ryzen 9 6900HX\nprocessor\t: 1\nmodel name\t: AMD Ryzen 9 6900HX\n";
        assert_eq!(parse_cpu_count(text), 2);
        assert_eq!(parse_cpu_model(text).as_deref(), Some("AMD Ryzen 9 6900HX"));
        assert_eq!(parse_cpu_count(""), 0);
        assert_eq!(parse_cpu_model(""), None);
        // "processor" inside another key is not a stanza.
        assert_eq!(parse_cpu_count("coprocessor: yes\n"), 0);
    }

    #[test]
    fn power_supply_reports_a_discharging_laptop() {
        let d = tmp("psu-bat");
        let bat = d.join("BAT0");
        put(&bat, "type", "Battery\n");
        put(&bat, "capacity", "45\n");
        put(&bat, "status", "Discharging\n");
        put(&bat, "energy_now", "20000000\n");
        put(&bat, "power_now", "5000000\n");
        let ac = d.join("AC");
        put(&ac, "type", "Mains\n");
        put(&ac, "online", "0\n");
        let out = parse_power_supply(&d).expect("parsed");
        assert_eq!(out["source"], "Battery Power");
        assert_eq!(out["on_ac"], false);
        assert_eq!(out["percent"], 45);
        assert_eq!(out["state"], "discharging");
        assert_eq!(out["time_remaining"], "4:00");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn power_supply_reports_mains_and_a_full_battery() {
        let d = tmp("psu-ac");
        let bat = d.join("BAT0");
        put(&bat, "type", "Battery\n");
        put(&bat, "capacity", "100\n");
        put(&bat, "status", "Full\n");
        let ac = d.join("ADP1");
        put(&ac, "type", "Mains\n");
        put(&ac, "online", "1\n");
        let out = parse_power_supply(&d).expect("parsed");
        assert_eq!(out["on_ac"], true);
        assert_eq!(out["percent"], 100);
        assert_eq!(out["state"], "charged");
        assert!(out.get("time_remaining").is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A desktop has an empty class directory: no power block at all.
    #[test]
    fn power_supply_without_supplies_yields_nothing() {
        let d = tmp("psu-none");
        assert!(parse_power_supply(&d).is_none());
        assert!(parse_power_supply(&d.join("missing")).is_none());
        // A battery whose capacity is garbage is not a battery we can report.
        let bat = d.join("BAT0");
        put(&bat, "type", "Battery\n");
        put(&bat, "capacity", "many\n");
        assert!(parse_power_supply(&d).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn thermal_zones_convert_millidegrees() {
        let d = tmp("thermal");
        put(&d.join("thermal_zone0"), "type", "acpitz\n");
        put(&d.join("thermal_zone0"), "temp", "20000\n");
        put(&d.join("thermal_zone1"), "type", "iwlwifi_1\n");
        put(&d.join("thermal_zone1"), "temp", "junk\n");
        put(&d.join("cooling_device0"), "type", "Fan\n");
        let z = parse_thermal(&d);
        assert_eq!(z.len(), 1, "{z:?}");
        assert_eq!(z[0]["type"], "acpitz");
        assert_eq!(z[0]["celsius"], 20.0);
        assert!(parse_thermal(&d.join("missing")).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn sysfs_usb_lists_devices_not_interfaces() {
        let d = tmp("usb");
        let dev = d.join("bus/usb/devices/3-1");
        put(&dev, "idVendor", "046d\n");
        put(&dev, "idProduct", "c548\n");
        put(&dev, "product", "USB Receiver\n");
        put(&dev, "manufacturer", "Logitech\n");
        put(&dev, "bDeviceClass", "00\n");
        // An interface directory has no idVendor and must be skipped.
        put(
            &d.join("bus/usb/devices/3-1:1.0"),
            "bInterfaceClass",
            "03\n",
        );
        let usb = sysfs_usb(&d);
        assert_eq!(usb.len(), 1, "{usb:?}");
        assert_eq!(usb[0]["id"], "3-1");
        assert_eq!(usb[0]["vendor_id"], "046d");
        assert_eq!(usb[0]["product_id"], "c548");
        assert_eq!(usb[0]["product"], "USB Receiver");
        assert_eq!(usb[0]["manufacturer"], "Logitech");
        assert_eq!(usb[0]["class"], "00");
        assert!(sysfs_usb(&d.join("nowhere")).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn sysfs_pci_reads_ids_and_driver() {
        let d = tmp("pci");
        let dev = d.join("bus/pci/devices/0000:00:01.2");
        put(&dev, "vendor", "0x1022\n");
        put(&dev, "device", "0x14ba\n");
        put(&dev, "class", "0x060400\n");
        std::os::unix::fs::symlink("../../../bus/pci/drivers/pcieport", dev.join("driver"))
            .unwrap();
        // A device with no vendor file is malformed and skipped.
        std::fs::create_dir_all(d.join("bus/pci/devices/0000:00:02.0")).unwrap();
        let pci = sysfs_pci(&d);
        assert_eq!(pci.len(), 1, "{pci:?}");
        assert_eq!(pci[0]["vendor_id"], "0x1022");
        assert_eq!(pci[0]["device_id"], "0x14ba");
        assert_eq!(pci[0]["class"], "0x060400");
        assert_eq!(pci[0]["driver"], "pcieport");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn sysfs_bluetooth_separates_adapters_from_devices() {
        let d = tmp("bt");
        put(
            &d.join("class/bluetooth/hci0"),
            "address",
            "AA:BB:CC:DD:EE:FF\n",
        );
        put(&d.join("class/bluetooth/hci0:256"), "name", "Keyboard\n");
        let bt = sysfs_bluetooth(&d);
        assert_eq!(bt.len(), 2);
        assert_eq!(bt[0]["kind"], "adapter");
        assert_eq!(bt[0]["address"], "AA:BB:CC:DD:EE:FF");
        assert_eq!(bt[1]["kind"], "device");
        assert_eq!(bt[1]["name"], "Keyboard");
        assert!(sysfs_bluetooth(&d.join("nowhere")).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn status_uid_is_the_real_uid() {
        assert_eq!(
            parse_status_uid("Name:\tcat\nUid:\t1000\t1000\t1000\t1000\nGid:\t1000\n"),
            Some(1000)
        );
        assert_eq!(parse_status_uid(""), None);
        assert_eq!(parse_status_uid("Uid:\n"), None);
        assert_eq!(parse_status_uid("Uid:\tx\n"), None);
    }

    #[test]
    fn maps_lines_parse_with_and_without_a_path() {
        let v = parse_maps_line(
            "55a21d305000-55a21d30c000 r-xp 00000000 00:23 13117826                   /usr/bin/head",
        )
        .unwrap();
        assert_eq!(v["start"], "0x55a21d305000");
        assert_eq!(v["end"], "0x55a21d30c000");
        assert_eq!(v["size"], 0x7000);
        assert_eq!(v["perms"], "r-xp");
        assert_eq!(v["path"], "/usr/bin/head");
        let anon = parse_maps_line("7ffd1a2b3000-7ffd1a2d4000 rw-p 00000000 00:00 0").unwrap();
        assert!(anon["path"].is_null());
        // Paths with spaces are kept whole.
        let sp = parse_maps_line("1000-2000 r--p 00000000 00:00 1 /tmp/a b.so").unwrap();
        assert_eq!(sp["path"], "/tmp/a b.so");
        assert!(parse_maps_line("").is_none());
        assert!(parse_maps_line("garbage").is_none());
        assert!(parse_maps_line("zz-1000 r--p 0 0 0").is_none());
    }

    #[test]
    fn sysctl_keys_map_onto_proc_sys_and_cannot_climb() {
        assert_eq!(
            sysctl_path("kern.osrelease"),
            Some(PathBuf::from("/proc/sys/kernel/osrelease"))
        );
        assert_eq!(
            sysctl_path("net.ipv4.ip_forward"),
            Some(PathBuf::from("/proc/sys/net/ipv4/ip_forward"))
        );
        assert_eq!(sysctl_path("kernel..osrelease"), None);
        assert_eq!(sysctl_path("kernel.osrelease."), None);
        assert_eq!(sysctl_path("..."), None);
        assert_eq!(sysctl_path(""), None);
        assert_eq!(sysctl_path("kernel/osrelease"), None);
    }

    #[test]
    fn regex_escape_makes_a_term_literal() {
        assert_eq!(regex_escape("abc 123"), "abc 123");
        assert_eq!(regex_escape("a.b*c"), "a\\.b\\*c");
        assert_eq!(regex_escape("(x)|[y]"), "\\(x\\)\\|\\[y\\]");
        assert_eq!(regex_escape(""), "");
    }

    /// Reading our own memory must work without any privilege, and the bytes
    /// must be the ones actually at that address.
    #[test]
    fn own_memory_reads_back_the_bytes_at_the_address() {
        static PROBE: [u8; 8] = *b"agentctl";
        let got = read_process_memory(
            std::process::id() as i32,
            PROBE.as_ptr() as u64,
            PROBE.len(),
        )
        .expect("own memory is readable");
        assert_eq!(got, b"agentctl");
        let err = read_process_memory(std::process::id() as i32, 0x10, 8).unwrap_err();
        assert!(err.contains("unmapped") || err.contains("failed"), "{err}");
    }

    #[test]
    fn linux_tool_names_the_missing_binary() {
        let e = linux_tool("definitely-not-a-real-tool-xyz").unwrap_err();
        assert!(e.contains("definitely-not-a-real-tool-xyz"), "{e}");
        assert!(e.contains("/usr/bin"), "{e}");
        // `sh` exists on every Linux box in one of the two places.
        assert!(linux_tool("sh").is_ok());
    }
}
