//! Linux backends for the filesystem engine: the trash, block devices and
//! mounts.
//!
//! Trash goes through GLib's `gio trash`, which writes the same freedesktop
//! trash entries the desktop's file manager reads, so "Put Back" works. GLib
//! refuses some locations (`/tmp` on a tmpfs, for one) and may be absent on a
//! headless box; then the freedesktop spec is implemented here directly into
//! the home trash, which is the fallback the spec itself allows.

use std::path::{Path, PathBuf};

use mcp_types::{Envelope, ErrorCode};
use serde_json::{json, Value};

use crate::tools::{run_fs_capture, valid_device, FsModule};

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

// ---- trash -----------------------------------------------------------------

/// `$XDG_DATA_HOME/Trash`, or `~/.local/share/Trash`.
pub(crate) fn home_trash_dir() -> Result<PathBuf, String> {
    if let Some(d) = std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(d).join("Trash"));
    }
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .ok_or("neither XDG_DATA_HOME nor HOME is set, so there is no trash directory")?;
    Ok(PathBuf::from(home).join(".local/share/Trash"))
}

/// The `Path=` line of a `.trashinfo` file: RFC 2396 percent-encoding of the
/// absolute path, with `/` kept.
pub(crate) fn encode_trash_path(p: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut out = String::new();
    for &b in p.as_os_str().as_bytes() {
        let keep = b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.' | b'~');
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Inverse of [`encode_trash_path`], for finding an entry again.
pub(crate) fn decode_trash_path(s: &str) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() && s.is_char_boundary(i + 3) {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    PathBuf::from(std::ffi::OsString::from_vec(out))
}

/// `Path=` from a `.trashinfo` body, decoded. `None` for anything that is not
/// a trashinfo file.
pub(crate) fn trashinfo_origin(body: &str) -> Option<PathBuf> {
    let mut lines = body.lines().map(str::trim);
    if lines.next()? != "[Trash Info]" {
        return None;
    }
    let raw = lines.find_map(|l| l.strip_prefix("Path="))?;
    (!raw.is_empty()).then(|| decode_trash_path(raw))
}

/// Local time as the spec wants it (`YYYY-MM-DDThh:mm:ss`), from `date`, with
/// UTC from the system clock as the fallback so an entry is never left without
/// a date.
fn deletion_date() -> String {
    if let Ok(date) = linux_tool("date") {
        if let Ok(o) = std::process::Command::new(date)
            .arg("+%Y-%m-%dT%H:%M:%S")
            .output()
        {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if o.status.success() && s.len() == 19 {
                return s;
            }
        }
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil-from-days (Howard Hinnant), proleptic Gregorian.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(src)?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        let target = std::fs::read_link(src)?;
        std::os::unix::fs::symlink(target, dst)
    } else if ft.is_dir() {
        std::fs::create_dir(dst)?;
        std::fs::set_permissions(dst, meta.permissions())?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            copy_tree(&e.path(), &dst.join(e.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(src, dst).map(|_| ())
    }
}

fn remove_any(p: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(p)?;
    if meta.is_dir() && !meta.file_type().is_symlink() {
        std::fs::remove_dir_all(p)
    } else {
        std::fs::remove_file(p)
    }
}

/// Move `path` into `trash` per the freedesktop trash specification: the
/// `.trashinfo` file is created first and exclusively (that is what reserves
/// the name), then the file is renamed in, or copied and removed when the
/// trash sits on another filesystem. Returns the path inside `files/`.
pub(crate) fn trash_into(path: &Path, trash: &Path) -> Result<PathBuf, String> {
    use std::os::unix::fs::DirBuilderExt;
    let files = trash.join("files");
    let info = trash.join("info");
    for d in [trash, &files, &info] {
        if !d.is_dir() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(d)
                .map_err(|e| format!("could not create {}: {e}", d.display()))?;
        }
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|n| !n.is_empty())
        .ok_or_else(|| format!("{} has no file name to trash under", path.display()))?;
    let body = format!(
        "[Trash Info]\nPath={}\nDeletionDate={}\n",
        encode_trash_path(path),
        deletion_date()
    );
    let mut chosen = None;
    for n in 0..10_000u32 {
        let candidate = if n == 0 {
            name.clone()
        } else {
            format!("{name}.{n}")
        };
        let info_path = info.join(format!("{candidate}.trashinfo"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&info_path)
        {
            Ok(mut f) => {
                use std::io::Write;
                if let Err(e) = f.write_all(body.as_bytes()) {
                    let _ = std::fs::remove_file(&info_path);
                    return Err(format!("could not write {}: {e}", info_path.display()));
                }
                chosen = Some((candidate, info_path));
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("could not create {}: {e}", info_path.display())),
        }
    }
    let Some((candidate, info_path)) = chosen else {
        return Err(format!(
            "the trash already holds 10000 entries named '{name}'"
        ));
    };
    let dest = files.join(&candidate);
    match std::fs::rename(path, &dest) {
        Ok(()) => Ok(dest),
        // EXDEV: the trash is on another filesystem, so copy then remove.
        Err(e) if e.raw_os_error() == Some(18) => {
            if let Err(e) = copy_tree(path, &dest) {
                let _ = remove_any(&dest);
                let _ = std::fs::remove_file(&info_path);
                return Err(format!("could not copy into the trash: {e}"));
            }
            if let Err(e) = remove_any(path) {
                let _ = remove_any(&dest);
                let _ = std::fs::remove_file(&info_path);
                return Err(format!(
                    "copied to the trash but could not remove the original: {e}"
                ));
            }
            Ok(dest)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&info_path);
            Err(format!("could not move into the trash: {e}"))
        }
    }
}

/// Trash directories the spec says an entry for `origin` could be in: the
/// home trash, and the per-volume ones at the top of its filesystem.
///
/// Each is paired with the volume top a per-volume trash records its paths
/// relative to (`None` for the home trash, whose paths are absolute).
fn candidate_trash_dirs(origin: &Path) -> Vec<(PathBuf, Option<PathBuf>)> {
    use std::os::unix::fs::MetadataExt;
    let mut out = Vec::new();
    if let Ok(h) = home_trash_dir() {
        out.push((h, None));
    }
    let uid = std::fs::metadata("/proc/self")
        .map(|m| m.uid())
        .ok()
        .or_else(|| std::env::var("UID").ok().and_then(|u| u.parse().ok()));
    let Some(uid) = uid else {
        return out;
    };
    // Walk up while the device number stays the same: the last directory on
    // the origin's filesystem is the top of its volume.
    let start = origin.parent().unwrap_or(origin);
    let Ok(dev) = std::fs::metadata(start).map(|m| m.dev()) else {
        return out;
    };
    let mut top = start.to_path_buf();
    while let Some(parent) = top.parent() {
        match std::fs::metadata(parent) {
            Ok(m) if m.dev() == dev => top = parent.to_path_buf(),
            _ => break,
        }
    }
    out.push((top.join(format!(".Trash-{uid}")), Some(top.clone())));
    out.push((top.join(".Trash").join(uid.to_string()), Some(top)));
    out
}

/// The entry in one trash directory whose `Path=` names `origin`, absolute or
/// relative to `topdir`. Only an entry whose file is really present counts.
pub(crate) fn locate_in_trash(
    trash: &Path,
    topdir: Option<&Path>,
    origin: &Path,
) -> Option<PathBuf> {
    let rd = std::fs::read_dir(trash.join("info")).ok()?;
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("trashinfo") {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(&p) else {
            continue;
        };
        let Some(recorded) = trashinfo_origin(&body) else {
            continue;
        };
        let matches = recorded == origin
            || (recorded.is_relative() && topdir.is_some_and(|t| t.join(&recorded) == origin));
        if !matches {
            continue;
        }
        let stem = p.file_stem()?.to_os_string();
        let dest = trash.join("files").join(stem);
        if std::fs::symlink_metadata(&dest).is_ok() {
            return Some(dest);
        }
    }
    None
}

/// Where a just-trashed `origin` ended up, by reading back the `.trashinfo`
/// entries. `None` when no entry names it, which is a real answer rather than
/// a guess.
pub(crate) fn locate_trashed(origin: &Path) -> Option<PathBuf> {
    candidate_trash_dirs(origin)
        .into_iter()
        .find_map(|(trash, top)| locate_in_trash(&trash, top.as_deref(), origin))
}

/// Trash a path: `gio trash` first, the home trash by hand when GLib refuses
/// or is absent. Returns where the entry landed when that can be read back.
pub(crate) fn move_to_trash(path: &Path) -> Result<Option<PathBuf>, String> {
    let gio_err = match linux_tool("gio") {
        Ok(gio) => {
            let out = std::process::Command::new(&gio)
                .arg("trash")
                .arg(path)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", std::env::var_os("HOME").unwrap_or_default())
                .env(
                    "XDG_DATA_HOME",
                    std::env::var_os("XDG_DATA_HOME").unwrap_or_default(),
                )
                .stdin(std::process::Stdio::null())
                .output()
                .map_err(|e| format!("gio: {e}"))?;
            if out.status.success() && !path.exists() && std::fs::symlink_metadata(path).is_err() {
                return Ok(locate_trashed(path));
            }
            String::from_utf8_lossy(&out.stderr).trim().to_string()
        }
        Err(e) => e,
    };
    tracing::debug!("gio trash declined ({gio_err}); using the home trash directly");
    let trash = home_trash_dir().map_err(|e| format!("{gio_err}; {e}"))?;
    trash_into(path, &trash)
        .map(Some)
        .map_err(|e| format!("gio: {gio_err}; home trash: {e}"))
}

// ---- storage and mounts ----------------------------------------------------

/// `lsblk --json` into the macOS `storage_inspect` device shape: one entry
/// per top-level block device with its partitions.
pub(crate) fn parse_lsblk(text: &str) -> Result<Vec<Value>, String> {
    let v: Value =
        serde_json::from_str(text).map_err(|e| format!("lsblk returned unparsable JSON: {e}"))?;
    let Some(devs) = v.get("blockdevices").and_then(Value::as_array) else {
        return Err("lsblk output has no 'blockdevices' array".into());
    };
    let str_of = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let mut out = Vec::new();
    for d in devs {
        let Some(name) = str_of(d, "name") else {
            continue;
        };
        let kind = str_of(d, "type").unwrap_or_default();
        let removable = std::fs::read_to_string(format!("/sys/block/{name}/removable"))
            .map(|s| s.trim() == "1")
            .unwrap_or(false);
        let mut partitions = Vec::new();
        let mut stack: Vec<&Value> = d
            .get("children")
            .and_then(Value::as_array)
            .map(|a| a.iter().rev().collect())
            .unwrap_or_default();
        while let Some(c) = stack.pop() {
            if let Some(ident) = str_of(c, "name") {
                partitions.push(json!({
                    "identifier": ident,
                    "type": str_of(c, "fstype").or_else(|| str_of(c, "type")).unwrap_or_default(),
                    "size": str_of(c, "size").unwrap_or_default(),
                    "mountpoint": c.get("mountpoint").cloned().unwrap_or(Value::Null),
                }));
            }
            if let Some(kids) = c.get("children").and_then(Value::as_array) {
                stack.extend(kids.iter().rev());
            }
        }
        out.push(json!({
            "device": format!("/dev/{name}"),
            "internal": !removable,
            "physical": kind == "disk",
            "type": kind,
            "size": str_of(d, "size").unwrap_or_default(),
            "model": d.get("model").cloned().unwrap_or(Value::Null),
            "fstype": d.get("fstype").cloned().unwrap_or(Value::Null),
            "mountpoint": d.get("mountpoint").cloned().unwrap_or(Value::Null),
            "partitions": partitions,
        }));
    }
    Ok(out)
}

/// One mounted filesystem from `findmnt --json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Mount {
    pub source: String,
    pub target: String,
    pub fstype: String,
    pub options: String,
}

/// `findmnt --json` (tree or `-l` list form) flattened to a list.
pub(crate) fn parse_findmnt(text: &str) -> Result<Vec<Mount>, String> {
    let v: Value =
        serde_json::from_str(text).map_err(|e| format!("findmnt returned unparsable JSON: {e}"))?;
    let Some(roots) = v.get("filesystems").and_then(Value::as_array) else {
        return Err("findmnt output has no 'filesystems' array".into());
    };
    let mut out = Vec::new();
    let mut stack: Vec<&Value> = roots.iter().rev().collect();
    while let Some(fs) = stack.pop() {
        let s = |k: &str| fs.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let target = s("target");
        if !target.is_empty() {
            out.push(Mount {
                source: s("source"),
                target,
                fstype: s("fstype"),
                options: s("options"),
            });
        }
        if let Some(kids) = fs.get("children").and_then(Value::as_array) {
            stack.extend(kids.iter().rev());
        }
    }
    Ok(out)
}

/// A mount target as `udisksctl` wants it: a block device path. Bare names
/// (`sda1`, `nvme0n1p2`) get `/dev/`; a mount point is resolved through the
/// mount table.
fn resolve_block_device(target: &str, mounts: &[Mount]) -> Option<String> {
    if target.starts_with("/dev/") {
        return Some(target.to_string());
    }
    if !target.starts_with('/') {
        return Some(format!("/dev/{target}"));
    }
    let t = target.trim_end_matches('/');
    let t = if t.is_empty() { "/" } else { t };
    mounts
        .iter()
        .find(|m| m.target == t && m.source.starts_with("/dev/"))
        // findmnt writes `/dev/sda1[/subvol]` for bind and subvolume mounts.
        .map(|m| m.source.split('[').next().unwrap_or(&m.source).to_string())
}

impl FsModule {
    pub(crate) async fn linux_storage_inspect(&self) -> Envelope {
        let tool = "storage_inspect";
        let lsblk = match linux_tool("lsblk") {
            Ok(p) => p,
            Err(e) => return Envelope::fail(tool, ErrorCode::UnsupportedOs, e),
        };
        let text = match run_fs_capture(
            &lsblk,
            &["--json", "-o", "NAME,SIZE,TYPE,MOUNTPOINT,FSTYPE,MODEL"],
        )
        .await
        {
            Ok(t) => t,
            Err(e) => return Envelope::fail(tool, ErrorCode::ActionFailed, e),
        };
        let devices = match parse_lsblk(&text) {
            Ok(d) => d,
            Err(e) => return Envelope::fail(tool, ErrorCode::ActionFailed, e),
        };
        let mounted: Vec<Value> = match linux_mounts().await {
            Ok(m) => m
                .into_iter()
                .map(|m| json!({ "device": m.source, "mount": m.target, "fs": m.fstype }))
                .collect(),
            Err(e) => {
                tracing::warn!("mount table unavailable: {e}");
                Vec::new()
            }
        };
        Envelope::ok(
            tool,
            json!({ "devices": devices, "mounted": mounted, "count": devices.len() }),
        )
    }

    pub(crate) async fn linux_mount_control(&self, args: &Value) -> Envelope {
        let tool = "mount_control";
        match args.get("action").and_then(Value::as_str).unwrap_or("list") {
            "list" => match linux_mounts().await {
                Ok(m) => {
                    let rows: Vec<Value> = m
                        .into_iter()
                        .map(|m| {
                            json!({ "device": m.source, "mount": m.target,
                                    "fs": m.fstype, "options": m.options })
                        })
                        .collect();
                    Envelope::ok(tool, json!({ "mounts": rows, "count": rows.len() }))
                }
                Err(e) => Envelope::fail(tool, ErrorCode::ActionFailed, e),
            },
            action @ ("mount" | "unmount") => {
                let Some(dev) = args
                    .get("device")
                    .or_else(|| args.get("mountpoint"))
                    .and_then(Value::as_str)
                else {
                    return Envelope::fail(
                        tool,
                        ErrorCode::InvalidArgs,
                        format!("'{action}' needs 'device' or 'mountpoint'"),
                    );
                };
                if !valid_device(dev) {
                    return Envelope::fail_with(
                        tool,
                        ErrorCode::InvalidArgs,
                        format!("'{dev}' is not a device identifier or mount point"),
                        "pass something like sda1, /dev/sda1, or /run/media/you/Name",
                    );
                }
                let udisksctl = match linux_tool("udisksctl") {
                    Ok(p) => p,
                    Err(e) => return Envelope::fail(tool, ErrorCode::ActionFailed, e),
                };
                let mounts = linux_mounts().await.unwrap_or_default();
                let Some(block) = resolve_block_device(dev, &mounts) else {
                    return Envelope::fail_with(
                        tool,
                        ErrorCode::NotFound,
                        format!("'{dev}' is not a mounted block device"),
                        "name the device (/dev/sda1) rather than a path that is not a mount point",
                    );
                };
                match run_fs_capture(&udisksctl, &[action, "--no-user-interaction", "-b", &block])
                    .await
                {
                    Ok(out) => Envelope::ok(
                        tool,
                        json!({ "ok": true, "action": action, "target": dev,
                                "device": block, "detail": out.trim() }),
                    ),
                    Err(e) => Envelope::fail(tool, ErrorCode::ActionFailed, e),
                }
            }
            other => Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                format!("unknown action '{other}' (list|mount|unmount)"),
            ),
        }
    }
}

/// The mount table from `findmnt`, with `/proc/self/mounts` as the fallback
/// when util-linux is missing.
async fn linux_mounts() -> Result<Vec<Mount>, String> {
    match linux_tool("findmnt") {
        Ok(findmnt) => {
            let text = run_fs_capture(&findmnt, &["--json", "-l"]).await?;
            parse_findmnt(&text)
        }
        Err(_) => {
            let text = std::fs::read_to_string("/proc/self/mounts")
                .map_err(|e| format!("/proc/self/mounts: {e}"))?;
            Ok(parse_proc_mounts(&text))
        }
    }
}

/// `/proc/self/mounts`: `source target fstype options dump pass`, with spaces
/// in paths written as `\040`.
pub(crate) fn parse_proc_mounts(text: &str) -> Vec<Mount> {
    fn unescape(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\' {
                let oct: String = chars.clone().take(3).collect();
                if oct.len() == 3 {
                    if let Ok(v) = u8::from_str_radix(&oct, 8) {
                        out.push(v as char);
                        for _ in 0..3 {
                            chars.next();
                        }
                        continue;
                    }
                }
            }
            out.push(c);
        }
        out
    }
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.len() >= 4).then(|| Mount {
                source: unescape(f[0]),
                target: unescape(f[1]),
                fstype: f[2].to_string(),
                options: f[3].to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mcp-fs-linux-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    const LSBLK: &str = r#"{
   "blockdevices": [
      {"name": "zram0", "size": "13.6G", "type": "disk", "mountpoint": "[SWAP]", "fstype": "swap", "model": null},
      {"name": "nvme0n1", "size": "931.5G", "type": "disk", "mountpoint": null, "fstype": null, "model": "CT1000P3PSSD8",
         "children": [
            {"name": "nvme0n1p1", "size": "600M", "type": "part", "mountpoint": "/boot/efi", "fstype": "vfat", "model": null},
            {"name": "nvme0n1p3", "size": "929.9G", "type": "part", "mountpoint": "/home", "fstype": "btrfs", "model": null}
         ]}
   ]
}"#;

    #[test]
    fn lsblk_maps_disks_and_partitions() {
        let d = parse_lsblk(LSBLK).unwrap();
        assert_eq!(d.len(), 2);
        assert_eq!(d[0]["device"], "/dev/zram0");
        assert_eq!(d[0]["physical"], true);
        assert_eq!(d[0]["partitions"].as_array().unwrap().len(), 0);
        assert_eq!(d[1]["device"], "/dev/nvme0n1");
        assert_eq!(d[1]["model"], "CT1000P3PSSD8");
        let parts = d[1]["partitions"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["identifier"], "nvme0n1p1");
        assert_eq!(parts[0]["type"], "vfat");
        assert_eq!(parts[0]["size"], "600M");
        assert_eq!(parts[1]["mountpoint"], "/home");
    }

    #[test]
    fn lsblk_rejects_garbage_and_tolerates_emptiness() {
        assert!(parse_lsblk("").is_err());
        assert!(parse_lsblk("not json").is_err());
        assert!(parse_lsblk("{}").is_err());
        assert_eq!(parse_lsblk(r#"{"blockdevices": []}"#).unwrap().len(), 0);
        // An entry with no name is skipped, not fatal.
        assert_eq!(
            parse_lsblk(r#"{"blockdevices": [{"size": "1G"}]}"#)
                .unwrap()
                .len(),
            0
        );
    }

    const FINDMNT_TREE: &str = r#"{"filesystems": [
      {"target": "/", "source": "/dev/nvme0n1p3[/root]", "fstype": "btrfs", "options": "rw,relatime",
       "children": [
          {"target": "/dev", "source": "devtmpfs", "fstype": "devtmpfs", "options": "rw,nosuid",
           "children": [{"target": "/dev/shm", "source": "tmpfs", "fstype": "tmpfs", "options": "rw"}]},
          {"target": "/home", "source": "/dev/nvme0n1p3[/home]", "fstype": "btrfs", "options": "rw"}
       ]}
   ]}"#;

    #[test]
    fn findmnt_flattens_the_tree_in_order() {
        let m = parse_findmnt(FINDMNT_TREE).unwrap();
        let targets: Vec<&str> = m.iter().map(|m| m.target.as_str()).collect();
        assert_eq!(targets, ["/", "/dev", "/dev/shm", "/home"]);
        assert_eq!(m[0].fstype, "btrfs");
        assert_eq!(m[0].options, "rw,relatime");
        // The list form has no children and parses the same way.
        let list = r#"{"filesystems": [{"target": "/", "source": "x", "fstype": "ext4", "options": "rw"}]}"#;
        assert_eq!(parse_findmnt(list).unwrap().len(), 1);
    }

    #[test]
    fn findmnt_rejects_garbage_and_tolerates_emptiness() {
        assert!(parse_findmnt("").is_err());
        assert!(parse_findmnt("[]").is_err());
        assert_eq!(parse_findmnt(r#"{"filesystems": []}"#).unwrap().len(), 0);
        // No target means no mount.
        assert_eq!(
            parse_findmnt(r#"{"filesystems": [{"source": "x"}]}"#)
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn proc_mounts_unescapes_octal_spaces() {
        let m = parse_proc_mounts(
            "/dev/sda1 /run/media/me/My\\040Disk vfat rw,relatime 0 0\nshort line\n\n",
        );
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].target, "/run/media/me/My Disk");
        assert_eq!(m[0].fstype, "vfat");
        assert!(parse_proc_mounts("").is_empty());
    }

    #[test]
    fn block_devices_resolve_from_names_paths_and_mount_points() {
        let mounts = vec![Mount {
            source: "/dev/nvme0n1p3[/home]".into(),
            target: "/home".into(),
            fstype: "btrfs".into(),
            options: "rw".into(),
        }];
        assert_eq!(
            resolve_block_device("sda1", &mounts).as_deref(),
            Some("/dev/sda1")
        );
        assert_eq!(
            resolve_block_device("/dev/sdb", &mounts).as_deref(),
            Some("/dev/sdb")
        );
        assert_eq!(
            resolve_block_device("/home", &mounts).as_deref(),
            Some("/dev/nvme0n1p3")
        );
        assert_eq!(
            resolve_block_device("/home/", &mounts).as_deref(),
            Some("/dev/nvme0n1p3")
        );
        assert_eq!(resolve_block_device("/nowhere", &mounts), None);
        assert_eq!(resolve_block_device("/nowhere", &[]), None);
    }

    #[test]
    fn trash_paths_round_trip_through_percent_encoding() {
        for p in [
            "/tmp/plain.txt",
            "/tmp/with space/a b.txt",
            "/tmp/ünïcode/ß.txt",
            "/tmp/100%.txt",
        ] {
            let enc = encode_trash_path(Path::new(p));
            assert!(!enc.contains(' '), "{enc}");
            assert_eq!(decode_trash_path(&enc), PathBuf::from(p), "{enc}");
        }
        assert_eq!(encode_trash_path(Path::new("/a b")), "/a%20b");
        assert_eq!(decode_trash_path(""), PathBuf::from(""));
        // A stray or truncated escape is kept literally, never a panic.
        assert_eq!(decode_trash_path("/a%2"), PathBuf::from("/a%2"));
        assert_eq!(decode_trash_path("/a%zz"), PathBuf::from("/a%zz"));
    }

    #[test]
    fn trashinfo_origin_is_read_only_from_a_real_entry() {
        assert_eq!(
            trashinfo_origin("[Trash Info]\nPath=/tmp/a%20b\nDeletionDate=2026-09-19T06:24:52\n"),
            Some(PathBuf::from("/tmp/a b"))
        );
        assert_eq!(trashinfo_origin(""), None);
        assert_eq!(trashinfo_origin("Path=/tmp/x\n"), None);
        assert_eq!(trashinfo_origin("[Trash Info]\nPath=\n"), None);
        assert_eq!(trashinfo_origin("[Trash Info]\nDeletionDate=x\n"), None);
    }

    #[test]
    fn deletion_date_has_the_spec_shape() {
        let d = deletion_date();
        assert_eq!(d.len(), 19, "{d}");
        assert_eq!(&d[4..5], "-");
        assert_eq!(&d[10..11], "T");
        assert!(d.starts_with("20"), "{d}");
    }

    /// The by-hand trash: entry reserved by the info file, file moved, and a
    /// second file of the same name gets a distinct slot.
    #[test]
    fn trash_into_moves_the_file_and_never_overwrites() {
        let d = tmp("trash-into");
        let trash = d.join("Trash");
        let src = d.join("victim.txt");
        std::fs::write(&src, "one").unwrap();
        let dest1 = trash_into(&src, &trash).unwrap();
        assert!(!src.exists());
        assert_eq!(std::fs::read_to_string(&dest1).unwrap(), "one");
        let info1 = trash.join("info/victim.txt.trashinfo");
        assert_eq!(
            trashinfo_origin(&std::fs::read_to_string(&info1).unwrap()),
            Some(src.clone())
        );

        std::fs::write(&src, "two").unwrap();
        let dest2 = trash_into(&src, &trash).unwrap();
        assert_ne!(dest1, dest2);
        assert_eq!(
            std::fs::read_to_string(&dest1).unwrap(),
            "one",
            "first entry untouched"
        );
        assert_eq!(std::fs::read_to_string(&dest2).unwrap(), "two");
        assert!(trash.join("info/victim.txt.1.trashinfo").exists());

        // A directory, with a symlink inside, survives too.
        let dir = d.join("victim-dir");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/f"), "deep").unwrap();
        std::os::unix::fs::symlink("sub/f", dir.join("link")).unwrap();
        let dest3 = trash_into(&dir, &trash).unwrap();
        assert!(!dir.exists());
        assert_eq!(
            std::fs::read_to_string(dest3.join("sub/f")).unwrap(),
            "deep"
        );
        assert!(std::fs::symlink_metadata(dest3.join("link"))
            .unwrap()
            .file_type()
            .is_symlink());

        // And an entry for the origin can be found again (two were made from
        // the same origin, so either is a correct answer).
        let found = locate_in_trash(&trash, None, &src).expect("an entry for the origin");
        assert!(found == dest1 || found == dest2, "{}", found.display());
        assert_eq!(
            locate_in_trash(&trash, None, &d.join("never-trashed")),
            None
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A per-volume trash records paths relative to the volume top; an entry
    /// whose file is missing does not count; an empty or absent trash is a
    /// clean miss.
    #[test]
    fn lookup_honours_relative_paths_and_ignores_dangling_entries() {
        let d = tmp("trash-lookup");
        let trash = d.join(".Trash-1000");
        std::fs::create_dir_all(trash.join("info")).unwrap();
        std::fs::create_dir_all(trash.join("files")).unwrap();
        std::fs::write(
            trash.join("info/doc.txt.trashinfo"),
            "[Trash Info]\nPath=docs/my%20doc.txt\nDeletionDate=2026-09-19T06:24:52\n",
        )
        .unwrap();
        std::fs::write(trash.join("files/doc.txt"), "x").unwrap();
        std::fs::write(
            trash.join("info/dangling.trashinfo"),
            "[Trash Info]\nPath=docs/dangling\nDeletionDate=2026-09-19T06:24:52\n",
        )
        .unwrap();
        let origin = d.join("docs/my doc.txt");
        assert_eq!(
            locate_in_trash(&trash, Some(&d), &origin),
            Some(trash.join("files/doc.txt"))
        );
        assert_eq!(
            locate_in_trash(&trash, None, &origin),
            None,
            "relative needs a top"
        );
        assert_eq!(
            locate_in_trash(&trash, Some(&d), &d.join("docs/dangling")),
            None
        );
        assert_eq!(
            locate_in_trash(&d.join("no-such-trash"), Some(&d), &origin),
            None
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn trash_into_reports_a_missing_source() {
        let d = tmp("trash-missing");
        let err = trash_into(&d.join("nope"), &d.join("Trash")).unwrap_err();
        assert!(err.contains("could not move"), "{err}");
        // The reserved info file is released again.
        assert!(std::fs::read_dir(d.join("Trash/info"))
            .unwrap()
            .next()
            .is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn trash_into_refuses_a_root_path() {
        let d = tmp("trash-root");
        let err = trash_into(Path::new("/"), &d.join("Trash")).unwrap_err();
        assert!(err.contains("no file name"), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn linux_tool_names_the_missing_binary() {
        let e = linux_tool("definitely-not-a-real-tool-xyz").unwrap_err();
        assert!(e.contains("definitely-not-a-real-tool-xyz"), "{e}");
        assert!(e.contains("/usr/bin"), "{e}");
        assert!(linux_tool("sh").is_ok());
    }
}
