use std::collections::BTreeMap;
use std::path::PathBuf;

use async_trait::async_trait;
use mcp_types::{CallCtx, Category, Envelope, ErrorCode, Tier, ToolDescriptor, ToolModule};
use serde_json::{json, Value};

use crate::jail::{Jail, PathError};

/// Filesystem tools, contained by a [`Jail`].
pub struct FsModule {
    jail: Jail,
    /// Largest file we will read into a response.
    max_read_bytes: u64,
    /// Cap on entries returned by list/search, so a huge tree cannot flood the
    /// agent's context.
    max_entries: usize,
}

impl FsModule {
    pub fn new(jail: Jail, max_read_bytes: u64, max_entries: usize) -> Self {
        FsModule {
            jail,
            max_read_bytes,
            max_entries,
        }
    }

    fn path_err(tool: &str, e: PathError) -> Envelope {
        let code = match e {
            PathError::Escapes(_) | PathError::Denied(_) => ErrorCode::PolicyDenied,
            PathError::Invalid(_) => ErrorCode::InvalidArgs,
        };
        Envelope::fail(tool, code, e.message())
    }

    fn io_err(tool: &str, e: std::io::Error) -> Envelope {
        let code = match e.kind() {
            std::io::ErrorKind::NotFound => ErrorCode::NotFound,
            std::io::ErrorKind::PermissionDenied => ErrorCode::PermDenied,
            _ => ErrorCode::ActionFailed,
        };
        Envelope::fail(tool, code, e.to_string())
    }

    fn arg_path(args: &Value, key: &str) -> Option<String> {
        args.get(key).and_then(Value::as_str).map(str::to_string)
    }

    fn read(&self, args: &Value) -> Envelope {
        let tool = "fs_read";
        let Some(p) = Self::arg_path(args, "path") else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'path'");
        };
        let path = match self.jail.resolve(&p) {
            Ok(x) => x,
            Err(e) => return Self::path_err(tool, e),
        };
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => return Self::io_err(tool, e),
        };
        if meta.is_dir() {
            return Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                "path is a directory; use fs_list",
            );
        }
        if meta.len() > self.max_read_bytes {
            return Envelope::fail_with(
                tool,
                ErrorCode::InvalidArgs,
                format!(
                    "file is {} bytes (limit {})",
                    meta.len(),
                    self.max_read_bytes
                ),
                "read a byte range with offset/length, or raise fs.max_read_bytes",
            );
        }
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => return Self::io_err(tool, e),
        };
        // Offer text when it is valid UTF-8; otherwise report binary rather than
        // emitting lossy garbage the agent would reason over.
        match String::from_utf8(bytes) {
            Ok(text) => Envelope::ok(
                tool,
                json!({ "path": path.display().to_string(), "bytes": meta.len(), "text": text }),
            ),
            Err(e) => Envelope::ok(
                tool,
                json!({
                    "path": path.display().to_string(),
                    "bytes": meta.len(),
                    "binary": true,
                    "note": format!("not valid UTF-8 (byte {}); content omitted", e.utf8_error().valid_up_to()),
                }),
            ),
        }
    }

    fn write(&self, args: &Value) -> Envelope {
        let tool = "fs_write";
        let Some(p) = Self::arg_path(args, "path") else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'path'");
        };
        let Some(content) = args.get("content").and_then(Value::as_str) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'content'");
        };
        let append = args.get("append").and_then(Value::as_bool).unwrap_or(false);
        let path = match self.jail.resolve(&p) {
            Ok(x) => x,
            Err(e) => return Self::path_err(tool, e),
        };
        let res = if append {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .and_then(|mut f| f.write_all(content.as_bytes()))
        } else {
            std::fs::write(&path, content)
        };
        match res {
            Ok(()) => Envelope::ok(
                tool,
                json!({ "path": path.display().to_string(), "bytes": content.len(), "append": append }),
            ),
            Err(e) => Self::io_err(tool, e),
        }
    }

    fn list(&self, args: &Value) -> Envelope {
        let tool = "fs_list";
        let p = Self::arg_path(args, "path").unwrap_or_else(|| {
            self.jail
                .roots()
                .first()
                .map(|r| r.display().to_string())
                .unwrap_or_default()
        });
        let path = match self.jail.resolve(&p) {
            Ok(x) => x,
            Err(e) => return Self::path_err(tool, e),
        };
        let rd = match std::fs::read_dir(&path) {
            Ok(r) => r,
            Err(e) => return Self::io_err(tool, e),
        };
        let mut entries = Vec::new();
        let mut truncated = false;
        for item in rd.flatten() {
            if entries.len() >= self.max_entries {
                truncated = true;
                break;
            }
            let meta = item.metadata().ok();
            entries.push(json!({
                "name": item.file_name().to_string_lossy(),
                "dir": meta.as_ref().map(|m| m.is_dir()).unwrap_or(false),
                "symlink": meta.as_ref().map(|m| m.file_type().is_symlink()).unwrap_or(false),
                "bytes": meta.as_ref().map(|m| m.len()).unwrap_or(0),
            }));
        }
        Envelope::ok(
            tool,
            json!({ "path": path.display().to_string(), "entries": entries, "truncated": truncated }),
        )
    }

    fn metadata(&self, args: &Value) -> Envelope {
        let tool = "fs_metadata";
        let Some(p) = Self::arg_path(args, "path") else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'path'");
        };
        let path = match self.jail.resolve(&p) {
            Ok(x) => x,
            Err(e) => return Self::path_err(tool, e),
        };
        match std::fs::symlink_metadata(&path) {
            Ok(m) => Envelope::ok(
                tool,
                json!({
                    "path": path.display().to_string(),
                    "bytes": m.len(),
                    "dir": m.is_dir(),
                    "file": m.is_file(),
                    "symlink": m.file_type().is_symlink(),
                    "readonly": m.permissions().readonly(),
                }),
            ),
            Err(e) => Self::io_err(tool, e),
        }
    }

    fn mkdir(&self, args: &Value) -> Envelope {
        let tool = "fs_mkdir";
        let Some(p) = Self::arg_path(args, "path") else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'path'");
        };
        let path = match self.jail.resolve(&p) {
            Ok(x) => x,
            Err(e) => return Self::path_err(tool, e),
        };
        match std::fs::create_dir_all(&path) {
            Ok(()) => Envelope::ok(tool, json!({ "path": path.display().to_string() })),
            Err(e) => Self::io_err(tool, e),
        }
    }

    /// Both endpoints of a move/copy are resolved and contained, otherwise a
    /// copy could be used to write outside the jail.
    fn transfer(&self, tool: &str, args: &Value, is_move: bool) -> Envelope {
        let (Some(from), Some(to)) = (Self::arg_path(args, "from"), Self::arg_path(args, "to"))
        else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'from' or 'to'");
        };
        let src = match self.jail.resolve(&from) {
            Ok(x) => x,
            Err(e) => return Self::path_err(tool, e),
        };
        let dst = match self.jail.resolve(&to) {
            Ok(x) => x,
            Err(e) => return Self::path_err(tool, e),
        };
        // Renaming a directory moves everything in it, protected files included.
        if is_move && self.jail.shields(&src) {
            return Envelope::fail(
                tool,
                ErrorCode::PolicyDenied,
                "refusing to move a directory that holds agentctl's own config, kill switch or audit log",
            );
        }
        let res = if is_move {
            std::fs::rename(&src, &dst)
        } else {
            std::fs::copy(&src, &dst).map(|_| ())
        };
        match res {
            Ok(()) => Envelope::ok(
                tool,
                json!({ "from": src.display().to_string(), "to": dst.display().to_string() }),
            ),
            Err(e) => Self::io_err(tool, e),
        }
    }

    fn delete(&self, args: &Value) -> Envelope {
        let tool = "fs_delete";
        let Some(p) = Self::arg_path(args, "path") else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'path'");
        };
        let recursive = args
            .get("recursive")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let path = match self.jail.resolve(&p) {
            Ok(x) => x,
            Err(e) => return Self::path_err(tool, e),
        };
        // Deleting a configured root would destroy the workspace itself.
        if self.jail.roots().iter().any(|r| r == &path) {
            return Envelope::fail(
                tool,
                ErrorCode::PolicyDenied,
                "refusing to delete a configured root",
            );
        }
        if self.jail.shields(&path) {
            return Envelope::fail(
                tool,
                ErrorCode::PolicyDenied,
                "refusing to delete a directory that holds agentctl's own config, kill switch or audit log",
            );
        }
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) => return Self::io_err(tool, e),
        };
        // Trash by default. Permanent deletion is the one filesystem action with
        // no undo, and an agent acting on a misread path should cost a restore
        // rather than the file.
        let trash = args.get("trash").and_then(Value::as_bool).unwrap_or(true);
        if trash {
            return match trash_path(&path) {
                Ok(location) => {
                    let mut data =
                        json!({ "trashed": path.display().to_string(), "recoverable": true });
                    if let Some(l) = location {
                        data["trash_location"] = json!(l.display().to_string());
                    }
                    Envelope::ok(tool, data)
                }
                Err(e) => Envelope::fail_with(
                    tool,
                    ErrorCode::ActionFailed,
                    e,
                    "pass trash=false to delete permanently instead",
                ),
            };
        }
        let res = if meta.is_dir() && !meta.file_type().is_symlink() {
            if recursive {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_dir(&path)
            }
        } else {
            std::fs::remove_file(&path)
        };
        match res {
            Ok(()) => Envelope::ok(
                tool,
                json!({ "deleted": path.display().to_string(), "recoverable": false }),
            ),
            Err(e) => Self::io_err(tool, e),
        }
    }

    /// Edit a file in place: literal find/replace pairs, or a unified diff.
    ///
    /// The point is *not* rewriting the whole file. A whole-file write races
    /// with anything else touching it and silently discards concurrent edits;
    /// a patch that cannot find its context fails loudly instead.
    fn patch(&self, args: &Value) -> Envelope {
        let tool = "fs_patch";
        let Some(p) = Self::arg_path(args, "path") else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'path'");
        };
        let path = match self.jail.resolve(&p) {
            Ok(x) => x,
            Err(e) => return Self::path_err(tool, e),
        };
        let original = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => return Self::io_err(tool, e),
        };

        let result = if let Some(edits) = args.get("edits").and_then(Value::as_array) {
            apply_edits(&original, edits)
        } else if let Some(diff) = args.get("patch").and_then(Value::as_str) {
            apply_unified_diff(&original, diff)
        } else {
            return Envelope::fail_with(
                tool,
                ErrorCode::InvalidArgs,
                "need either 'edits' or 'patch'",
                "edits: [{find, replace, count?}] for literal replacement, or a unified diff",
            );
        };
        let (updated, applied) = match result {
            Ok(v) => v,
            Err(e) => return Envelope::fail(tool, ErrorCode::InvalidArgs, e),
        };
        // Write via a temp file in the same directory, then rename: an
        // interrupted patch must not leave the file half-written.
        let tmp = path.with_extension(format!(
            "{}.agentctl-tmp",
            path.extension().and_then(|e| e.to_str()).unwrap_or("")
        ));
        if let Err(e) = std::fs::write(&tmp, &updated) {
            return Self::io_err(tool, e);
        }
        if let Err(e) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Self::io_err(tool, e);
        }
        Envelope::ok(
            tool,
            json!({
                "applied": applied, "path": path.display().to_string(),
                "bytes": updated.len(),
                "lines_before": original.lines().count(),
                "lines_after": updated.lines().count(),
            }),
        )
    }

    /// Create a symlink. Both the link *and its target* must be inside the
    /// roots: a link pointing out would otherwise become a permanent hole in
    /// the jail that every later read walks straight through.
    fn symlink(&self, args: &Value) -> Envelope {
        let tool = "fs_symlink";
        let (Some(target), Some(link)) =
            (Self::arg_path(args, "target"), Self::arg_path(args, "link"))
        else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "need 'target' and 'link'");
        };
        let target = match self.jail.resolve(&target) {
            Ok(x) => x,
            Err(e) => return Self::path_err(tool, e),
        };
        let link = match self.jail.resolve(&link) {
            Ok(x) => x,
            Err(e) => return Self::path_err(tool, e),
        };
        if link.exists() {
            return Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                format!("'{}' already exists", link.display()),
            );
        }
        // Windows splits the call by target kind and needs the distinction up
        // front, so the target is classified before the link is created.
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&target, &link);
        #[cfg(windows)]
        let made = if target.is_dir() {
            std::os::windows::fs::symlink_dir(&target, &link)
        } else {
            std::os::windows::fs::symlink_file(&target, &link)
        };
        match made {
            Ok(()) => Envelope::ok(
                tool,
                json!({ "link": link.display().to_string(), "target": target.display().to_string() }),
            ),
            Err(e) => Self::io_err(tool, e),
        }
    }

    /// Compress or extract an archive. Both endpoints are jailed, and extraction
    /// is verified afterwards: an archive can carry `../` entries or absolute
    /// paths that would write outside the destination.
    async fn archive(&self, args: &Value) -> Envelope {
        let tool = "fs_archive";
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("compress");
        let (Some(src), Some(dst)) = (Self::arg_path(args, "src"), Self::arg_path(args, "dst"))
        else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "need 'src' and 'dst'");
        };
        let src = match self.jail.resolve(&src) {
            Ok(x) => x,
            Err(e) => return Self::path_err(tool, e),
        };
        let dst = match self.jail.resolve(&dst) {
            Ok(x) => x,
            Err(e) => return Self::path_err(tool, e),
        };
        let format = args
            .get("format")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| guess_format(&dst));
        if action == "compress" {
            if let Some(bad) = first_refused(&self.jail, &src) {
                return Envelope::fail(
                    tool,
                    ErrorCode::PolicyDenied,
                    format!("refusing to archive a tree that holds a denied path: {bad}"),
                );
            }
        }
        if action == "extract" {
            return self.extract_staged(tool, &src, &dst, &format).await;
        }
        let (s, d) = (src.display().to_string(), dst.display().to_string());

        let result = match (action, format.as_str()) {
            ("compress", "zip") => {
                let parent = src.parent().map(|p| p.to_path_buf());
                let name = src
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| ".".into());
                // -y stores a symlink as a link: following it would pack whatever it
                // points at, jail or no jail.
                run_fs_tool("/usr/bin/zip", &["-qry", &d, &name], parent.as_deref()).await
            }
            ("compress", "tar.gz") => {
                run_fs_tool(
                    "/usr/bin/tar",
                    &["-czf", &d, "-C", &parent_of(&src), &leaf_of(&src)],
                    None,
                )
                .await
            }
            ("compress", "tar.xz") => {
                run_fs_tool(
                    "/usr/bin/tar",
                    &["-cJf", &d, "-C", &parent_of(&src), &leaf_of(&src)],
                    None,
                )
                .await
            }
            (a, f) => Err(format!("cannot {a} format '{f}'")),
        };
        if let Err(e) = result {
            return Envelope::fail(tool, ErrorCode::ActionFailed, e);
        }
        Envelope::ok(
            tool,
            json!({ "action": action, "format": format, "src": s, "dst": d }),
        )
    }

    /// Extract into a staging directory inside `dst`, check every entry the
    /// archive produced against the jail, and only then move them into place.
    ///
    /// Extracting straight into `dst` and checking afterwards leaves the damage
    /// done: an archive carrying `.ssh/authorized_keys` or `.agentctl/config.toml`
    /// would land before any check ran. A refused archive changes nothing.
    async fn extract_staged(
        &self,
        tool: &str,
        src: &std::path::Path,
        dst: &std::path::Path,
        format: &str,
    ) -> Envelope {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let staging = dst.join(format!(
            ".fs-archive-staging-{}-{nanos}",
            std::process::id()
        ));
        if let Err(e) = std::fs::create_dir_all(&staging) {
            return Self::io_err(tool, e);
        }
        let (s, st) = (src.display().to_string(), staging.display().to_string());
        let result = if format == "zip" {
            // -o overwrite, and unzip refuses absolute paths itself.
            run_fs_tool("/usr/bin/unzip", &["-qo", &s, "-d", &st], None).await
        } else {
            run_fs_tool("/usr/bin/tar", &["-xf", &s, "-C", &st], None).await
        };
        let outcome = result
            .map_err(|e| (ErrorCode::ActionFailed, e))
            .and_then(|_| match first_escape(&staging, self.jail.roots()) {
                Some(escaped) => Err((
                    ErrorCode::PolicyDenied,
                    format!("archive wrote outside the allowed roots: {escaped}"),
                )),
                None => Ok(()),
            })
            .and_then(|_| self.place_staged(&staging, dst));
        let _ = std::fs::remove_dir_all(&staging);
        match outcome {
            Ok(n) => Envelope::ok(
                tool,
                json!({ "action": "extract", "format": format, "src": s,
                        "dst": dst.display().to_string(), "entries": n }),
            ),
            Err((code, e)) => {
                let hint = if code == ErrorCode::PolicyDenied {
                    "nothing was extracted; the destination is unchanged"
                } else {
                    "inspect the destination; some entries may already be in place"
                };
                Envelope::fail_with(tool, code, e, hint)
            }
        }
    }

    /// Check every staged entry, then move them all into `dst`. Returns the
    /// number of entries placed.
    fn place_staged(
        &self,
        staging: &std::path::Path,
        dst: &std::path::Path,
    ) -> Result<usize, (ErrorCode, String)> {
        let denied = |m: String| (ErrorCode::PolicyDenied, m);
        let mut entries: Vec<(PathBuf, bool)> = Vec::new();
        let mut stack = vec![staging.to_path_buf()];
        while let Some(d) = stack.pop() {
            let rd = std::fs::read_dir(&d).map_err(|e| (ErrorCode::ActionFailed, e.to_string()))?;
            for entry in rd.flatten() {
                let p = entry.path();
                let ft = entry
                    .file_type()
                    .map_err(|e| (ErrorCode::ActionFailed, e.to_string()))?;
                let rel = p.strip_prefix(staging).unwrap_or(&p).to_path_buf();
                // A link would let a later write land wherever it points.
                if !ft.is_dir() && !ft.is_file() {
                    return Err(denied(format!(
                        "archive holds a link or special file: {}",
                        rel.display()
                    )));
                }
                let target = dst.join(&rel);
                self.jail
                    .resolve(&target.to_string_lossy())
                    .map_err(|e| denied(e.message()))?;
                if ft.is_dir() {
                    stack.push(p);
                }
                entries.push((rel, ft.is_dir()));
                if entries.len() > 20_000 {
                    return Err(denied("archive holds more than 20000 entries".into()));
                }
            }
        }
        // Parents sort before their children.
        entries.sort();
        for (rel, is_dir) in &entries {
            let (from, to) = (staging.join(rel), dst.join(rel));
            let res = if *is_dir {
                std::fs::create_dir_all(&to)
            } else {
                std::fs::rename(&from, &to)
            };
            res.map_err(|e| (ErrorCode::ActionFailed, format!("{}: {e}", rel.display())))?;
        }
        Ok(entries.len())
    }

    /// Watch a tree for changes and report the delta.
    ///
    /// Polls a metadata snapshot rather than binding FSEvents: the snapshot is
    /// portable, needs no framework, and this returns a bounded delta rather
    /// than a live stream, so the cheap approach is also the right shape.
    async fn watch(&self, args: &Value) -> Envelope {
        let tool = "fs_watch";
        let Some(root) = Self::arg_path(args, "root") else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'root'");
        };
        let root = match self.jail.resolve(&root) {
            Ok(x) => x,
            Err(e) => return Self::path_err(tool, e),
        };
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(5000)
            .clamp(100, 60_000);
        let before = snapshot_tree(&root, self.max_entries);
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        let mut after;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            after = snapshot_tree(&root, self.max_entries);
            if after != before || std::time::Instant::now() >= deadline {
                break;
            }
        }
        let (created, modified, deleted) = diff_trees(&before, &after);
        Envelope::ok(
            tool,
            json!({
                "root": root.display().to_string(),
                "created": created, "modified": modified, "deleted": deleted,
                "changed": !(created.is_empty() && modified.is_empty() && deleted.is_empty()),
                "watched_ms": timeout_ms,
            }),
        )
    }

    /// Physical devices and volumes. Read-only.
    #[cfg(not(target_os = "linux"))]
    async fn storage_inspect(&self) -> Envelope {
        let tool = "storage_inspect";
        let list = match run_fs_capture("/usr/sbin/diskutil", &["list"]).await {
            Ok(t) => t,
            Err(e) => return Envelope::fail(tool, ErrorCode::UnsupportedOs, e),
        };
        let mut devices: Vec<Value> = Vec::new();
        let mut current: Option<Value> = None;
        for line in list.lines() {
            let t = line.trim();
            if t.starts_with("/dev/disk") {
                if let Some(d) = current.take() {
                    devices.push(d);
                }
                let internal = t.contains("internal");
                current = Some(json!({
                    "device": t.split_whitespace().next().unwrap_or(t),
                    "internal": internal,
                    "physical": t.contains("physical"),
                    "partitions": Vec::<Value>::new(),
                }));
            } else if let Some(dev) = current.as_mut() {
                // "   1:  Apple_APFS Container disk3  494.4 GB   disk0s2"
                let cols: Vec<&str> = t.split_whitespace().collect();
                if cols.len() >= 3 && cols[0].ends_with(':') {
                    let ident = cols[cols.len() - 1];
                    let size = format!("{} {}", cols[cols.len() - 3], cols[cols.len() - 2]);
                    dev["partitions"].as_array_mut().unwrap().push(json!({
                        "identifier": ident,
                        "type": cols[1],
                        "size": size,
                    }));
                }
            }
        }
        if let Some(d) = current.take() {
            devices.push(d);
        }
        let mounts = run_fs_capture("/sbin/mount", &[]).await.unwrap_or_default();
        let mounted: Vec<Value> = mounts
            .lines()
            .filter_map(|l| {
                let (dev, rest) = l.split_once(" on ")?;
                let (mount, kind) = rest.split_once(" (")?;
                Some(json!({
                    "device": dev, "mount": mount,
                    "fs": kind.trim_end_matches(')').split(',').next().unwrap_or("")
                }))
            })
            .collect();
        Envelope::ok(
            tool,
            json!({ "devices": devices, "mounted": mounted, "count": devices.len() }),
        )
    }

    /// Mount and unmount volumes. Unmounting a volume takes it away from every
    /// other program on the machine, not just this session.
    #[cfg(not(target_os = "linux"))]
    async fn mount_control(&self, args: &Value) -> Envelope {
        let tool = "mount_control";
        match args.get("action").and_then(Value::as_str).unwrap_or("list") {
            "list" => {
                let text = match run_fs_capture("/sbin/mount", &[]).await {
                    Ok(t) => t,
                    Err(e) => return Envelope::fail(tool, ErrorCode::ActionFailed, e),
                };
                let rows: Vec<Value> = text
                    .lines()
                    .filter_map(|l| {
                        let (dev, rest) = l.split_once(" on ")?;
                        let (mount, opts) = rest.split_once(" (")?;
                        Some(json!({ "device": dev, "mount": mount,
                                     "options": opts.trim_end_matches(')') }))
                    })
                    .collect();
                Envelope::ok(tool, json!({ "mounts": rows, "count": rows.len() }))
            }
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
                        "pass something like disk3s1, /dev/disk3s1, or /Volumes/Name",
                    );
                }
                match run_fs_capture("/usr/sbin/diskutil", &[action, dev]).await {
                    Ok(out) => Envelope::ok(
                        tool,
                        json!({ "ok": true, "action": action, "target": dev, "detail": out.trim() }),
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

    /// Recursive content search. Bounded in depth, matches, and file size so a
    /// large tree cannot hang the call or flood the response.
    fn search(&self, args: &Value) -> Envelope {
        let tool = "fs_search";
        let Some(query) = args.get("query").and_then(Value::as_str) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'query'");
        };
        if query.is_empty() {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "'query' must not be empty");
        }
        let p = Self::arg_path(args, "path").unwrap_or_else(|| {
            self.jail
                .roots()
                .first()
                .map(|r| r.display().to_string())
                .unwrap_or_default()
        });
        let root = match self.jail.resolve(&p) {
            Ok(x) => x,
            Err(e) => return Self::path_err(tool, e),
        };
        let names_only = args
            .get("names_only")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut hits = Vec::new();
        let mut scanned = 0usize;
        walk(&root, 0, 12, &mut |file: &std::path::Path| {
            if hits.len() >= self.max_entries {
                return false;
            }
            // The walk starts inside the jail but does not stay checked: a root
            // of `~` holds `.ssh/` and `.agentctl/`, whose lines must not come
            // back as search hits.
            if self.jail.resolve(&file.to_string_lossy()).is_err() {
                return true;
            }
            scanned += 1;
            let name = file
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if names_only {
                if name.contains(query) {
                    hits.push(json!({ "path": file.display().to_string() }));
                }
                return true;
            }
            let Ok(meta) = std::fs::metadata(file) else {
                return true;
            };
            if meta.len() > self.max_read_bytes {
                return true;
            }
            if let Ok(text) = std::fs::read_to_string(file) {
                for (i, line) in text.lines().enumerate() {
                    if line.contains(query) {
                        hits.push(json!({
                            "path": file.display().to_string(),
                            "line": i + 1,
                            "text": line.chars().take(200).collect::<String>(),
                        }));
                        break;
                    }
                }
            }
            true
        });
        let truncated = hits.len() >= self.max_entries;
        Envelope::ok(
            tool,
            json!({ "query": query, "root": root.display().to_string(),
                    "matches": hits, "files_scanned": scanned, "truncated": truncated }),
        )
    }
}

/// Depth-bounded walk. `f` returns false to stop early. Symlinked directories
/// are not followed: that is both a loop hazard and a jail escape.
fn walk(
    dir: &std::path::Path,
    depth: usize,
    max_depth: usize,
    f: &mut impl FnMut(&std::path::Path) -> bool,
) {
    if depth > max_depth {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for item in rd.flatten() {
        let path = item.path();
        let Ok(ft) = item.file_type() else { continue };
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            walk(&path, depth + 1, max_depth, f);
        } else if !f(&path) {
            return;
        }
    }
}

/// Move a path to the Trash.
///
/// Through Finder rather than a rename into `~/.Trash`, because Finder records
/// the Put Back location: the difference between "recoverable" and "somewhere
/// in a folder of orphans".
#[cfg(target_os = "macos")]
fn move_to_trash(path: &std::path::Path) -> Result<(), String> {
    let script = format!(
        "tell application \"Finder\" to delete POSIX file \"{}\"",
        mcp_policy::applescript_escape(&path.display().to_string())
    );
    let out = std::process::Command::new("/usr/bin/osascript")
        .args(["-e", &script])
        .output()
        .map_err(|e| format!("osascript: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "could not move to Trash: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Trash `path`, returning where it landed when the platform can say. Finder
/// does not report the destination; the freedesktop trash does.
#[cfg(target_os = "macos")]
fn trash_path(path: &std::path::Path) -> Result<Option<PathBuf>, String> {
    move_to_trash(path).map(|()| None)
}

#[cfg(target_os = "linux")]
fn trash_path(path: &std::path::Path) -> Result<Option<PathBuf>, String> {
    crate::linux::move_to_trash(path)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn trash_path(_path: &std::path::Path) -> Result<Option<PathBuf>, String> {
    Err("moving to the trash is implemented for macOS and Linux only".into())
}

/// Apply literal find/replace edits. Every `find` must match, and an ambiguous
/// match is an error: a patch that silently hits the wrong occurrence is worse
/// than one that refuses.
fn apply_edits(original: &str, edits: &[Value]) -> Result<(String, usize), String> {
    let mut text = original.to_string();
    let mut applied = 0usize;
    for (i, e) in edits.iter().enumerate() {
        let find = e
            .get("find")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("edit {i}: missing 'find'"))?;
        if find.is_empty() {
            return Err(format!("edit {i}: 'find' must not be empty"));
        }
        let replace = e.get("replace").and_then(Value::as_str).unwrap_or("");
        let hits = text.matches(find).count();
        if hits == 0 {
            return Err(format!(
                "edit {i}: no match for {find:?}. The file does not contain the text this patch \
                 expects, so nothing was changed"
            ));
        }
        let want = e.get("count").and_then(Value::as_u64).unwrap_or(1) as usize;
        if want == 0 {
            // count: 0 means "every occurrence".
            text = text.replace(find, replace);
            applied += hits;
            continue;
        }
        if hits != want {
            return Err(format!(
                "edit {i}: {find:?} matches {hits} times but count says {want}; refusing to guess \
                 which one you meant"
            ));
        }
        text = text.replacen(find, replace, want);
        applied += want;
    }
    Ok((text, applied))
}

/// Apply a unified diff.
///
/// Context lines are *verified*, not skipped: if the file has moved on since the
/// diff was made, the hunk must fail rather than land in the wrong place.
fn apply_unified_diff(original: &str, diff: &str) -> Result<(String, usize), String> {
    let src: Vec<&str> = original.split_inclusive('\n').collect();
    let mut out: Vec<String> = Vec::new();
    let mut cursor = 0usize;
    let mut hunks = 0usize;
    let mut lines = diff.lines().peekable();

    while let Some(line) = lines.next() {
        if line.starts_with("---") || line.starts_with("+++") || line.starts_with("diff ") {
            continue;
        }
        if !line.starts_with("@@") {
            continue;
        }
        // "@@ -old,len +new,len @@"
        let start: usize = line
            .split_whitespace()
            .nth(1)
            .and_then(|s| {
                s.trim_start_matches('-')
                    .split(',')
                    .next()
                    .map(str::to_string)
            })
            .and_then(|s| s.parse::<usize>().ok())
            .ok_or_else(|| format!("malformed hunk header: {line}"))?;
        let target = start.saturating_sub(1);
        if target < cursor {
            return Err(format!("hunk at line {start} overlaps an earlier hunk"));
        }
        if target > src.len() {
            return Err(format!("hunk at line {start} is past the end of the file"));
        }
        for l in &src[cursor..target] {
            out.push((*l).to_string());
        }
        cursor = target;
        hunks += 1;

        while let Some(body) = lines.peek() {
            if body.starts_with("@@") || body.starts_with("--- ") || body.starts_with("+++ ") {
                break;
            }
            let body = lines.next().unwrap();
            let (tag, content) =
                body.split_at(body.char_indices().nth(1).map_or(body.len(), |(i, _)| i));
            match tag {
                "+" => out.push(format!("{content}\n")),
                "-" | " " => {
                    let actual = src.get(cursor).ok_or_else(|| {
                        format!("hunk runs past the end of the file at line {}", cursor + 1)
                    })?;
                    if actual.trim_end_matches(['\n', '\r']) != content {
                        return Err(format!(
                            "context mismatch at line {}: patch expects {:?}, file has {:?}. The \
                             file changed since this diff was made",
                            cursor + 1,
                            content,
                            actual.trim_end_matches(['\n', '\r'])
                        ));
                    }
                    if tag == " " {
                        out.push((*actual).to_string());
                    }
                    cursor += 1;
                }
                "\\" => {}
                _ => break,
            }
        }
    }
    if hunks == 0 {
        return Err("no hunks found (is this a unified diff?)".into());
    }
    for l in &src[cursor..] {
        out.push((*l).to_string());
    }
    Ok((out.concat(), hunks))
}

fn parent_of(p: &std::path::Path) -> String {
    p.parent()
        .map(|x| x.display().to_string())
        .unwrap_or_else(|| ".".into())
}

fn leaf_of(p: &std::path::Path) -> String {
    p.file_name()
        .map(|x| x.to_string_lossy().to_string())
        .unwrap_or_else(|| ".".into())
}

fn guess_format(dst: &std::path::Path) -> String {
    let name = dst.to_string_lossy().to_ascii_lowercase();
    if name.ends_with(".zip") {
        "zip".into()
    } else if name.ends_with(".tar.xz") || name.ends_with(".txz") {
        "tar.xz".into()
    } else {
        "tar.gz".into()
    }
}

pub(crate) fn valid_device(d: &str) -> bool {
    !d.is_empty()
        && d.len() < 128
        && !d.starts_with('-')
        && d.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.' | ' '))
        && !d.contains("..")
}

async fn run_fs_tool(
    program: &str,
    args: &[&str],
    cwd: Option<&std::path::Path>,
) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .stdin(std::process::Stdio::null());
    if let Some(c) = cwd {
        cmd.current_dir(c);
    }
    let out = cmd.output().await.map_err(|e| format!("{program}: {e}"))?;
    if !out.status.success() {
        let e = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if e.is_empty() {
            format!("{program} exited {}", out.status)
        } else {
            e
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

pub(crate) async fn run_fs_capture(program: &str, args: &[&str]) -> Result<String, String> {
    run_fs_tool(program, args, None).await
}

/// Path → (size, mtime) for every regular file under `root`, bounded.
fn snapshot_tree(root: &std::path::Path, max: usize) -> BTreeMap<String, (u64, u64)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if out.len() >= max {
            break;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(p);
                continue;
            }
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            out.insert(p.display().to_string(), (meta.len(), mtime));
            if out.len() >= max {
                break;
            }
        }
    }
    out
}

type Tree = BTreeMap<String, (u64, u64)>;

fn diff_trees(before: &Tree, after: &Tree) -> (Vec<String>, Vec<String>, Vec<String>) {
    let created = after
        .keys()
        .filter(|k| !before.contains_key(*k))
        .cloned()
        .collect();
    let deleted = before
        .keys()
        .filter(|k| !after.contains_key(*k))
        .cloned()
        .collect();
    let modified = after
        .iter()
        .filter(|(k, v)| before.get(*k).is_some_and(|b| b != *v))
        .map(|(k, _)| k.clone())
        .collect();
    (created, modified, deleted)
}

/// First path under `dir` that resolves outside every root. An archive can
/// carry `../` entries; this catches what the extractor let through.
/// The first path at or under `dir` the jail refuses, without following links.
/// Fails closed past 20000 entries.
fn first_refused(jail: &Jail, dir: &std::path::Path) -> Option<String> {
    if jail.shields(dir) {
        return Some(dir.display().to_string());
    }
    let mut stack = vec![dir.to_path_buf()];
    let mut seen = 0usize;
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in rd.flatten() {
            seen += 1;
            if seen > 20_000 {
                return Some(format!("more than 20000 entries under {}", dir.display()));
            }
            let p = entry.path();
            if jail.resolve(&p.to_string_lossy()).is_err() {
                return Some(p.display().to_string());
            }
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                stack.push(p);
            }
        }
    }
    None
}

fn first_escape(dir: &std::path::Path, roots: &[PathBuf]) -> Option<String> {
    let mut stack = vec![dir.to_path_buf()];
    let mut seen = 0usize;
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in rd.flatten() {
            seen += 1;
            if seen > 20_000 {
                return None;
            }
            let p = entry.path();
            let real = std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
            if !roots.iter().any(|r| real.starts_with(r)) {
                return Some(real.display().to_string());
            }
            if entry.metadata().map(|m| m.is_dir()).unwrap_or(false) {
                stack.push(p);
            }
        }
    }
    None
}

#[async_trait]
impl ToolModule for FsModule {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        let p = |extra: Value, required: Value| {
            let mut props = json!({ "path": { "type": "string" } });
            if let (Some(o), Some(e)) = (props.as_object_mut(), extra.as_object()) {
                for (k, v) in e {
                    o.insert(k.clone(), v.clone());
                }
            }
            json!({ "type": "object", "properties": props, "required": required })
        };
        vec![
            ToolDescriptor::new(
                "fs_read",
                Category::Filesystem,
                Tier::Read,
                "Read a UTF-8 text file inside the allowed roots.",
                p(json!({}), json!(["path"])),
            ).untrusted_output(),
            ToolDescriptor::new(
                "fs_list",
                Category::Filesystem,
                Tier::Read,
                "List a directory inside the allowed roots.",
                p(json!({}), json!([])),
            ).untrusted_output(),
            ToolDescriptor::new(
                "fs_metadata",
                Category::Filesystem,
                Tier::Read,
                "Size/type/permission metadata for a path (does not follow symlinks).",
                p(json!({}), json!(["path"])),
            ),
            ToolDescriptor::new(
                "fs_patch",
                Category::Filesystem,
                Tier::Standard,
                "Edit a file in place with literal find/replace pairs or a unified diff. Prefer \
                 this over fs_write for changes: a whole-file rewrite discards anything else that \
                 touched the file, while a patch whose context no longer matches fails loudly.",
                json!({"type":"object","properties":{
                    "path":{"type":"string"},
                    "edits":{"type":"array","items":{"type":"object","properties":{
                        "find":{"type":"string"},"replace":{"type":"string"},
                        "count":{"type":"integer","description":"expected matches; 0 = replace all"}},
                        "required":["find"]}},
                    "patch":{"type":"string","description":"a unified diff"}},
                    "required":["path"]}),
            ),
            ToolDescriptor::new(
                "fs_symlink",
                Category::Filesystem,
                Tier::Standard,
                "Create a symlink. Both the link and its target must be inside the allowed roots.",
                json!({"type":"object","properties":{
                    "target":{"type":"string"},"link":{"type":"string"}},
                    "required":["target","link"]}),
            ),
            ToolDescriptor::new(
                "fs_archive",
                Category::Filesystem,
                Tier::Standard,
                "Compress or extract zip/tar.gz/tar.xz. Extraction is verified afterwards, since \
                 an archive can carry paths that point outside the destination.",
                json!({"type":"object","properties":{
                    "action":{"type":"string","enum":["compress","extract"]},
                    "src":{"type":"string"},"dst":{"type":"string"},
                    "format":{"type":"string","enum":["zip","tar.gz","tar.xz"]}},
                    "required":["action","src","dst"]}),
            ).untrusted_output(),
            ToolDescriptor::new(
                "fs_watch",
                Category::Filesystem,
                Tier::Read,
                "Block until something under a directory changes, then report created, modified \
                 and deleted paths. Returns at the timeout even if nothing changed.",
                json!({"type":"object","properties":{
                    "root":{"type":"string"},"timeout_ms":{"type":"integer"}},
                    "required":["root"]}),
            ),
            ToolDescriptor::new(
                "storage_inspect",
                Category::Filesystem,
                Tier::Read,
                "Physical devices, their partitions, and what is mounted where.",
                json!({"type":"object","properties":{},"required":[]}),
            ),
            ToolDescriptor::new(
                "mount_control",
                Category::Filesystem,
                Tier::Dangerous,
                "List, mount or unmount volumes. Unmounting takes a volume away from every program \
                 on the machine, not just this session.",
                json!({"type":"object","properties":{
                    "action":{"type":"string","enum":["list","mount","unmount"]},
                    "device":{"type":"string"},"mountpoint":{"type":"string"}},
                    "required":["action"]}),
            ),
            ToolDescriptor::new(
                "fs_search",
                Category::Filesystem,
                Tier::Read,
                "Search file contents (or names) recursively under an allowed root.",
                p(
                    json!({ "query": { "type": "string" },
                          "names_only": { "type": "boolean" } }),
                    json!(["query"]),
                ),
            ).untrusted_output(),
            ToolDescriptor::new(
                "fs_write",
                Category::Filesystem,
                Tier::Standard,
                "Write (or append to) a file inside the allowed roots.",
                p(
                    json!({ "content": { "type": "string" },
                          "append": { "type": "boolean" } }),
                    json!(["path", "content"]),
                ),
            ),
            ToolDescriptor::new(
                "fs_mkdir",
                Category::Filesystem,
                Tier::Standard,
                "Create a directory (and parents) inside the allowed roots.",
                p(json!({}), json!(["path"])),
            ).idempotent(true),
            ToolDescriptor::new(
                "fs_copy",
                Category::Filesystem,
                Tier::Standard,
                "Copy a file; both endpoints must be inside the allowed roots.",
                json!({ "type": "object", "properties": {
                    "from": { "type": "string" }, "to": { "type": "string" } },
                    "required": ["from", "to"] }),
            ),
            ToolDescriptor::new(
                "fs_move",
                Category::Filesystem,
                Tier::Standard,
                "Move/rename a file; both endpoints must be inside the allowed roots.",
                json!({ "type": "object", "properties": {
                    "from": { "type": "string" }, "to": { "type": "string" } },
                    "required": ["from", "to"] }),
            ),
            ToolDescriptor::new(
                "fs_delete",
                Category::Filesystem,
                Tier::Dangerous,
                "Delete a file or directory. Irreversible.",
                p(
                    json!({ "recursive": { "type": "boolean" } }),
                    json!(["path"]),
                ),
            ),
        ]
    }

    async fn call(&self, name: &str, args: Value, _ctx: &CallCtx) -> Envelope {
        if self.jail.is_empty() {
            return Envelope::fail_with(
                name,
                ErrorCode::PolicyDenied,
                "no filesystem roots are configured",
                "set fs.roots in config.toml to the directories the agent may touch",
            );
        }
        match name {
            "fs_read" => self.read(&args),
            "fs_write" => self.write(&args),
            "fs_list" => self.list(&args),
            "fs_metadata" => self.metadata(&args),
            "fs_mkdir" => self.mkdir(&args),
            "fs_copy" => self.transfer("fs_copy", &args, false),
            "fs_move" => self.transfer("fs_move", &args, true),
            "fs_delete" => self.delete(&args),
            "fs_search" => self.search(&args),
            "fs_patch" => self.patch(&args),
            "fs_symlink" => self.symlink(&args),
            "fs_archive" => self.archive(&args).await,
            "fs_watch" => self.watch(&args).await,
            #[cfg(not(target_os = "linux"))]
            "storage_inspect" => self.storage_inspect().await,
            #[cfg(not(target_os = "linux"))]
            "mount_control" => self.mount_control(&args).await,
            #[cfg(target_os = "linux")]
            "storage_inspect" => self.linux_storage_inspect().await,
            #[cfg(target_os = "linux")]
            "mount_control" => self.linux_mount_control(&args).await,
            other => Envelope::fail(other, ErrorCode::InvalidArgs, "unknown tool"),
        }
    }

    /// Deletion is irreversible, so a human approves the specific target.
    /// Recursive deletes say so explicitly: that is the one people regret.
    fn consent_prompt(&self, name: &str, args: &Value) -> Option<String> {
        let path = args.get("path").and_then(Value::as_str).unwrap_or("?");
        match name {
            "fs_delete" => {
                let recursive = args
                    .get("recursive")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                // Only permanent deletion is irreversible; trashing is not, and
                // saying "cannot be undone" about a recoverable action teaches
                // people to click through the prompt that matters.
                let permanent = args.get("trash").and_then(Value::as_bool) == Some(false);
                Some(match (recursive, permanent) {
                    (true, true) => format!(
                        "PERMANENTLY delete '{path}' and everything under it? This cannot be undone."
                    ),
                    (false, true) => format!("PERMANENTLY delete '{path}'? This cannot be undone."),
                    (true, _) => format!("Move '{path}' and everything under it to the Trash?"),
                    (false, _) => format!("Move '{path}' to the Trash?"),
                })
            }
            "mount_control" => {
                let action = args.get("action").and_then(Value::as_str).unwrap_or("list");
                let target = args
                    .get("device")
                    .or_else(|| args.get("mountpoint"))
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                matches!(action, "mount" | "unmount").then(|| {
                    format!("{action} '{target}'? This affects every program on the machine.")
                })
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jail::default_denied;
    use std::path::PathBuf;

    fn setup(tag: &str) -> (FsModule, PathBuf) {
        let root = std::env::temp_dir().join(format!("mcp-fs-tools-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::write(root.join("a.txt"), "hello needle world").unwrap();
        std::fs::write(root.join("nested/b.txt"), "no match here").unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        let jail = Jail::new(vec![root.clone()], default_denied());
        (FsModule::new(jail, 1_000_000, 100), root)
    }

    fn ctx() -> CallCtx {
        CallCtx::new("t", mcp_types::CancelToken::new())
    }

    #[tokio::test]
    async fn reads_and_writes_inside_the_root() {
        let (m, root) = setup("rw");
        let env = m
            .call(
                "fs_read",
                json!({ "path": root.join("a.txt").to_str().unwrap() }),
                &ctx(),
            )
            .await;
        assert!(env.ok, "{env:?}");
        assert_eq!(env.data.unwrap()["text"], "hello needle world");

        let target = root.join("new.txt");
        let env = m
            .call(
                "fs_write",
                json!({ "path": target.to_str().unwrap(), "content": "written" }),
                &ctx(),
            )
            .await;
        assert!(env.ok, "{env:?}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "written");
    }

    /// The core guarantee: no argument may reach outside the roots.
    #[tokio::test]
    async fn every_tool_refuses_a_path_outside_the_root() {
        let (m, _) = setup("escape");
        let cases: Vec<(&str, Value)> = vec![
            ("fs_read", json!({ "path": "/etc/passwd" })),
            (
                "fs_write",
                json!({ "path": "/tmp/evil-escape.txt", "content": "x" }),
            ),
            ("fs_list", json!({ "path": "/etc" })),
            ("fs_metadata", json!({ "path": "/etc/passwd" })),
            ("fs_mkdir", json!({ "path": "/tmp/evil-dir" })),
            ("fs_delete", json!({ "path": "/etc/passwd" })),
            ("fs_search", json!({ "query": "x", "path": "/etc" })),
            ("fs_copy", json!({ "from": "/etc/passwd", "to": "/tmp/x" })),
            ("fs_move", json!({ "from": "/etc/passwd", "to": "/tmp/x" })),
        ];
        for (tool, args) in cases {
            let env = m.call(tool, args, &ctx()).await;
            assert!(!env.ok, "{tool} allowed an escaping path");
            assert_eq!(
                env.error.unwrap().code,
                ErrorCode::PolicyDenied,
                "{tool} should be policy-denied"
            );
        }
        assert!(!std::path::Path::new("/tmp/evil-escape.txt").exists());
        assert!(!std::path::Path::new("/tmp/evil-dir").exists());
    }

    /// A copy's *destination* must be contained too, not just its source.
    #[tokio::test]
    async fn copy_destination_is_also_contained() {
        let (m, root) = setup("copydst");
        let env = m
            .call(
                "fs_copy",
                json!({ "from": root.join("a.txt").to_str().unwrap(), "to": "/tmp/leaked.txt" }),
                &ctx(),
            )
            .await;
        assert!(!env.ok);
        assert!(!std::path::Path::new("/tmp/leaked.txt").exists());
    }

    #[tokio::test]
    async fn search_finds_content_and_reports_the_line() {
        let (m, _) = setup("search");
        let env = m
            .call("fs_search", json!({ "query": "needle" }), &ctx())
            .await;
        assert!(env.ok, "{env:?}");
        let d = env.data.unwrap();
        let matches = d["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1, "{matches:?}");
        assert_eq!(matches[0]["line"], 1);
    }

    #[tokio::test]
    async fn refuses_to_delete_a_configured_root() {
        let (m, root) = setup("delroot");
        let env = m
            .call(
                "fs_delete",
                json!({ "path": root.to_str().unwrap(), "recursive": true }),
                &ctx(),
            )
            .await;
        assert!(!env.ok);
        assert!(root.exists(), "root must survive");
    }

    /// The server's own files, relocated inside a root (as `$AGENTCTL_CONFIG`,
    /// `kill_switch_file` and `audit_dir` allow), stay out of the agent's
    /// reach: by name, by case alias, and by moving or trashing their folder.
    #[tokio::test]
    async fn the_servers_own_files_are_protected_wherever_they_live() {
        let (_, root) = setup("ownstate");
        let ops = root.join("ops");
        std::fs::create_dir_all(ops.join("audit")).unwrap();
        std::fs::write(ops.join("agentctl.toml"), "[policy]\n").unwrap();
        std::fs::write(ops.join("notes.txt"), "free").unwrap();
        let jail = Jail::new(vec![root.clone()], default_denied()).with_protected([
            ops.join("agentctl.toml"),
            ops.join("STOP"), // not created yet, as usual
            ops.join("audit"),
        ]);
        let m = FsModule::new(jail, 1_000_000, 100);
        let s = |p: PathBuf| p.to_str().unwrap().to_string();
        let refused: Vec<(&str, Value)> = vec![
            (
                "fs_write",
                json!({ "path": s(ops.join("agentctl.toml")), "content": "[policy]\nmode = \"bypass\"\n" }),
            ),
            (
                "fs_write",
                json!({ "path": s(ops.join("AGENTCTL.TOML")), "content": "x" }),
            ),
            ("fs_read", json!({ "path": s(ops.join("agentctl.toml")) })),
            (
                "fs_write",
                json!({ "path": s(ops.join("STOP")), "content": "" }),
            ),
            (
                "fs_write",
                json!({ "path": s(ops.join("audit/log.jsonl")), "content": "{}" }),
            ),
            (
                "fs_delete",
                json!({ "path": s(ops.join("audit")), "recursive": true, "trash": false }),
            ),
            (
                "fs_delete",
                json!({ "path": s(ops.clone()), "recursive": true }),
            ),
            (
                "fs_delete",
                json!({ "path": s(ops.clone()), "recursive": true, "trash": false }),
            ),
            (
                "fs_move",
                json!({ "from": s(ops.clone()), "to": s(root.join("elsewhere")) }),
            ),
            (
                "fs_copy",
                json!({ "from": s(root.join("a.txt")), "to": s(ops.join("agentctl.toml")) }),
            ),
        ];
        for (tool, args) in refused {
            let env = m.call(tool, args.clone(), &ctx()).await;
            assert!(!env.ok, "{tool} {args} reached a protected path");
        }
        assert_eq!(
            std::fs::read_to_string(ops.join("agentctl.toml")).unwrap(),
            "[policy]\n"
        );
        assert!(ops.join("audit").is_dir());
        assert!(!ops.join("STOP").exists());

        // Neighbours stay usable: protection is by path component, not prefix.
        let allowed: Vec<(&str, Value)> = vec![
            (
                "fs_write",
                json!({ "path": s(ops.join("notes.txt")), "content": "still free" }),
            ),
            (
                "fs_write",
                json!({ "path": s(ops.join("agentctl.toml.bak")), "content": "x" }),
            ),
            ("fs_list", json!({ "path": s(ops.clone()) })),
            (
                "fs_delete",
                json!({ "path": s(ops.join("notes.txt")), "trash": false }),
            ),
        ];
        for (tool, args) in allowed {
            let env = m.call(tool, args.clone(), &ctx()).await;
            assert!(env.ok, "{tool} {args} was refused: {env:?}");
        }
    }

    /// Deletes must ask a human, name the target, and distinguish recoverable
    /// from permanent: calling a trash move "cannot be undone" trains people to
    /// click through the prompt that actually matters.
    #[test]
    fn delete_consent_names_the_target_and_the_stakes() {
        let (m, _) = setup("consent");
        assert!(m
            .consent_prompt("fs_read", &json!({ "path": "/x" }))
            .is_none());

        let trashed = m
            .consent_prompt("fs_delete", &json!({ "path": "/a/b", "recursive": true }))
            .unwrap();
        assert!(trashed.contains("/a/b"), "{trashed}");
        assert!(trashed.contains("Trash"), "{trashed}");
        assert!(trashed.contains("everything under it"), "{trashed}");
        assert!(
            !trashed.contains("cannot be undone"),
            "trashing is recoverable: {trashed}"
        );

        let permanent = m
            .consent_prompt(
                "fs_delete",
                &json!({ "path": "/a/b", "recursive": true, "trash": false }),
            )
            .unwrap();
        assert!(permanent.contains("PERMANENTLY"), "{permanent}");
        assert!(permanent.contains("cannot be undone"), "{permanent}");
    }

    /// The default has to be the recoverable one.
    #[tokio::test]
    async fn delete_moves_to_the_trash_unless_told_otherwise() {
        let (m, root) = setup("trash");
        let f = root.join("scratch.txt");
        std::fs::write(&f, b"bye").unwrap();
        let env = m
            .call(
                "fs_delete",
                json!({ "path": f.display().to_string() }),
                &ctx(),
            )
            .await;
        assert!(env.ok, "{env:?}");
        let d = env.data.unwrap();
        assert_eq!(d["recoverable"], true, "default delete must be recoverable");
        assert!(d.get("trashed").is_some(), "{d}");
        assert!(!f.exists(), "file must be gone from its original location");
        // The freedesktop trash says where the entry went; it must really be
        // there, with its content, and the test takes it out again so a run
        // does not fill the developer's wastebasket.
        #[cfg(target_os = "linux")]
        {
            let location = PathBuf::from(
                d["trash_location"]
                    .as_str()
                    .unwrap_or_else(|| panic!("no trash_location in {d}")),
            );
            assert_eq!(
                std::fs::read_to_string(&location).unwrap(),
                "bye",
                "the trashed file must be readable at {}",
                location.display()
            );
            let files = location.parent().unwrap();
            assert_eq!(
                files.file_name().unwrap(),
                "files",
                "{}",
                location.display()
            );
            let info = files.parent().unwrap().join("info").join(format!(
                "{}.trashinfo",
                location.file_name().unwrap().to_string_lossy()
            ));
            assert!(info.exists(), "no trashinfo at {}", info.display());
            std::fs::remove_file(&location).unwrap();
            std::fs::remove_file(&info).unwrap();
        }

        let g = root.join("gone.txt");
        std::fs::write(&g, b"bye").unwrap();
        let env = m
            .call(
                "fs_delete",
                json!({ "path": g.display().to_string(), "trash": false }),
                &ctx(),
            )
            .await;
        assert!(env.ok, "{env:?}");
        assert_eq!(env.data.unwrap()["recoverable"], false);
        assert!(!g.exists());
    }

    #[test]
    fn only_mounting_actions_ask() {
        let (m, _) = setup("mountconsent");
        assert!(m
            .consent_prompt("mount_control", &json!({ "action": "list" }))
            .is_none());
        let p = m
            .consent_prompt(
                "mount_control",
                &json!({ "action": "unmount", "device": "disk9" }),
            )
            .unwrap();
        assert!(p.contains("disk9"), "{p}");
    }

    #[tokio::test]
    async fn oversized_reads_are_refused_with_guidance() {
        let root = std::env::temp_dir().join(format!("mcp-fs-big-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("big.bin"), vec![b'x'; 5000]).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        let m = FsModule::new(Jail::new(vec![root.clone()], default_denied()), 1000, 100);
        let env = m
            .call(
                "fs_read",
                json!({ "path": root.join("big.bin").to_str().unwrap() }),
                &ctx(),
            )
            .await;
        assert!(!env.ok);
        assert!(env.error.unwrap().suggestion.is_some());
    }

    #[tokio::test]
    async fn with_no_roots_everything_is_denied() {
        let m = FsModule::new(Jail::new(vec![], default_denied()), 1000, 10);
        let env = m
            .call("fs_read", json!({ "path": "/etc/passwd" }), &ctx())
            .await;
        assert!(!env.ok);
        assert_eq!(env.error.unwrap().code, ErrorCode::PolicyDenied);
    }
}

#[cfg(test)]
mod extended_tests {
    use super::*;
    use crate::jail::default_denied;

    fn setup(tag: &str) -> (FsModule, PathBuf) {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("mcp-fs-ext-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        (
            FsModule::new(
                Jail::new(vec![root.clone()], default_denied()),
                1_000_000,
                500,
            ),
            root,
        )
    }
    fn ctx() -> CallCtx {
        CallCtx::new("t", mcp_types::CancelToken::new())
    }

    // ---- fs_patch ----------------------------------------------------------

    #[tokio::test]
    async fn patch_applies_literal_edits() {
        let (m, root) = setup("patch");
        let f = root.join("a.txt");
        std::fs::write(&f, "hello world\nsecond line\n").unwrap();
        let env = m
            .call(
                "fs_patch",
                json!({ "path": f.display().to_string(),
                        "edits": [{ "find": "world", "replace": "there" }] }),
                &ctx(),
            )
            .await;
        assert!(env.ok, "{env:?}");
        assert_eq!(env.data.unwrap()["applied"], 1);
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            "hello there\nsecond line\n"
        );
    }

    /// An edit whose text is not there means the file is not what the caller
    /// thought. Writing anyway would be worse than failing.
    #[tokio::test]
    async fn patch_refuses_when_the_text_is_not_found() {
        let (m, root) = setup("patchmiss");
        let f = root.join("a.txt");
        std::fs::write(&f, "hello\n").unwrap();
        let env = m
            .call(
                "fs_patch",
                json!({ "path": f.display().to_string(),
                        "edits": [{ "find": "not present", "replace": "x" }] }),
                &ctx(),
            )
            .await;
        assert!(!env.ok);
        assert!(env.error.unwrap().message.contains("no match"));
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            "hello\n",
            "file untouched"
        );
    }

    /// An ambiguous find would silently hit the wrong occurrence.
    #[tokio::test]
    async fn patch_refuses_an_ambiguous_match() {
        let (m, root) = setup("patchambig");
        let f = root.join("a.txt");
        std::fs::write(&f, "x\nx\nx\n").unwrap();
        let env = m
            .call(
                "fs_patch",
                json!({ "path": f.display().to_string(), "edits": [{ "find": "x", "replace": "y" }] }),
                &ctx(),
            )
            .await;
        assert!(!env.ok);
        assert!(env.error.unwrap().message.contains("matches 3 times"));

        // count: 0 means "all", which is unambiguous.
        let env = m
            .call(
                "fs_patch",
                json!({ "path": f.display().to_string(),
                        "edits": [{ "find": "x", "replace": "y", "count": 0 }] }),
                &ctx(),
            )
            .await;
        assert!(env.ok, "{env:?}");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "y\ny\ny\n");
    }

    #[tokio::test]
    async fn patch_applies_a_unified_diff() {
        let (m, root) = setup("diff");
        let f = root.join("a.txt");
        std::fs::write(&f, "one\ntwo\nthree\n").unwrap();
        let diff = "--- a\n+++ b\n@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n";
        let env = m
            .call(
                "fs_patch",
                json!({ "path": f.display().to_string(), "patch": diff }),
                &ctx(),
            )
            .await;
        assert!(env.ok, "{env:?}");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "one\nTWO\nthree\n");
    }

    /// The context check is the whole safety property of a diff.
    #[tokio::test]
    async fn patch_refuses_a_diff_whose_context_moved() {
        let (m, root) = setup("diffstale");
        let f = root.join("a.txt");
        std::fs::write(&f, "one\nCHANGED\nthree\n").unwrap();
        let diff = "@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n";
        let env = m
            .call(
                "fs_patch",
                json!({ "path": f.display().to_string(), "patch": diff }),
                &ctx(),
            )
            .await;
        assert!(!env.ok);
        assert!(env.error.unwrap().message.contains("context mismatch"));
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            "one\nCHANGED\nthree\n"
        );
    }

    #[tokio::test]
    async fn patch_outside_the_root_is_denied() {
        let (m, _) = setup("patchjail");
        let env = m
            .call(
                "fs_patch",
                json!({ "path": "/etc/hosts", "edits": [{ "find": "a", "replace": "b" }] }),
                &ctx(),
            )
            .await;
        assert!(!env.ok);
        assert_eq!(env.error.unwrap().code, ErrorCode::PolicyDenied);
    }

    // ---- fs_symlink --------------------------------------------------------

    /// A link pointing out of the jail would be a permanent hole every later
    /// read walks through.
    #[tokio::test]
    async fn symlink_target_must_also_be_contained() {
        let (m, root) = setup("symlink");
        let target = root.join("real.txt");
        std::fs::write(&target, b"x").unwrap();

        let ok = m
            .call(
                "fs_symlink",
                json!({ "target": target.display().to_string(),
                        "link": root.join("link.txt").display().to_string() }),
                &ctx(),
            )
            .await;
        assert!(ok.ok, "{ok:?}");
        assert!(root.join("link.txt").exists());

        let escape = m
            .call(
                "fs_symlink",
                json!({ "target": "/etc/passwd",
                        "link": root.join("escape.txt").display().to_string() }),
                &ctx(),
            )
            .await;
        assert!(!escape.ok, "a link out of the jail must be refused");
        assert_eq!(escape.error.unwrap().code, ErrorCode::PolicyDenied);
        assert!(!root.join("escape.txt").exists());
    }

    // ---- fs_archive --------------------------------------------------------

    #[tokio::test]
    async fn archive_round_trips_through_zip_and_tar() {
        for (fmt, ext) in [("zip", "zip"), ("tar.gz", "tar.gz")] {
            let (m, root) = setup(&format!("arch-{ext}"));
            let src = root.join("payload");
            std::fs::create_dir_all(&src).unwrap();
            std::fs::write(src.join("f.txt"), b"contents").unwrap();
            let archive = root.join(format!("out.{ext}"));

            let c = m
                .call(
                    "fs_archive",
                    json!({ "action": "compress", "src": src.display().to_string(),
                            "dst": archive.display().to_string(), "format": fmt }),
                    &ctx(),
                )
                .await;
            assert!(c.ok, "{fmt}: {c:?}");
            assert!(archive.exists(), "{fmt}: no archive written");

            let out = root.join("extracted");
            std::fs::create_dir_all(&out).unwrap();
            let x = m
                .call(
                    "fs_archive",
                    json!({ "action": "extract", "src": archive.display().to_string(),
                            "dst": out.display().to_string(), "format": fmt }),
                    &ctx(),
                )
                .await;
            assert!(x.ok, "{fmt}: {x:?}");
            assert_eq!(
                std::fs::read_to_string(out.join("payload/f.txt")).unwrap(),
                "contents",
                "{fmt}: round trip lost the file"
            );
        }
    }

    /// Build a tar outside any jail, the way an attacker would hand one over.
    fn tar_of(tag: &str, build: impl FnOnce(&std::path::Path), member: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("mcp-fs-evil-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("tree")).unwrap();
        build(&dir.join("tree"));
        let out = dir.join("evil.tar");
        let st = std::process::Command::new("tar")
            .arg("-cf")
            .arg(&out)
            .arg("-C")
            .arg(dir.join("tree"))
            .arg(member)
            .status()
            .unwrap();
        assert!(st.success());
        out
    }

    fn leftovers(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with(".fs-archive-staging"))
            .collect()
    }

    #[tokio::test]
    async fn search_does_not_read_denied_files_under_the_root() {
        let (m, root) = setup("searchdeny");
        std::fs::create_dir_all(root.join(".ssh")).unwrap();
        std::fs::write(root.join(".ssh/id_ed25519"), "BEGIN OPENSSH PRIVATE KEY").unwrap();
        std::fs::write(root.join("notes.txt"), "BEGIN OPENSSH notes").unwrap();
        let env = m
            .call(
                "fs_search",
                json!({ "query": "BEGIN OPENSSH", "path": root.display().to_string() }),
                &ctx(),
            )
            .await;
        assert!(env.ok, "{env:?}");
        let text = serde_json::to_string(&env.data).unwrap();
        assert!(text.contains("notes.txt"), "{text}");
        assert!(!text.contains(".ssh"), "search read a denied file: {text}");
    }

    #[tokio::test]
    async fn compress_refuses_a_tree_holding_a_denied_path() {
        let (m, root) = setup("compressdeny");
        std::fs::create_dir_all(root.join("home/.ssh")).unwrap();
        std::fs::write(root.join("home/.ssh/id_ed25519"), "key").unwrap();
        std::fs::write(root.join("home/ok.txt"), "ok").unwrap();
        for fmt in ["zip", "tar.gz"] {
            let out = root.join(format!("bundle.{fmt}"));
            let env = m
                .call(
                    "fs_archive",
                    json!({ "action": "compress", "src": root.join("home").display().to_string(),
                            "dst": out.display().to_string(), "format": fmt }),
                    &ctx(),
                )
                .await;
            assert!(!env.ok, "{fmt} packed a credential store");
            assert!(!out.exists(), "{fmt}: archive written anyway");
        }
    }

    /// An archive is checked before anything lands, so a refused one leaves
    /// the destination exactly as it was.
    #[tokio::test]
    async fn extract_refuses_an_archive_that_writes_a_denied_path() {
        let (m, root) = setup("extractdeny");
        let evil = tar_of(
            "creds",
            |t| {
                std::fs::create_dir_all(t.join(".ssh")).unwrap();
                std::fs::write(t.join(".ssh/authorized_keys"), "ssh-ed25519 AAAA attacker")
                    .unwrap();
            },
            ".ssh",
        );
        let env = m
            .call(
                "fs_archive",
                json!({ "action": "extract", "src": evil.display().to_string(),
                        "dst": root.display().to_string(), "format": "tar" }),
                &ctx(),
            )
            .await;
        assert!(!env.ok, "extracted into .ssh: {env:?}");
        let _ = std::fs::copy(&evil, root.join("evil.tar"));
        let env = m
            .call(
                "fs_archive",
                json!({ "action": "extract", "src": root.join("evil.tar").display().to_string(),
                        "dst": root.display().to_string(), "format": "tar" }),
                &ctx(),
            )
            .await;
        assert!(!env.ok, "extracted into .ssh: {env:?}");
        assert!(!root.join(".ssh").exists(), "a refused archive still wrote");
        assert!(leftovers(&root).is_empty(), "staging left behind");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn extract_refuses_an_archive_holding_a_link() {
        let (m, root) = setup("extractlink");
        let evil = tar_of(
            "link",
            |t| {
                std::fs::create_dir_all(t.join("pkg")).unwrap();
                std::os::unix::fs::symlink("/etc", t.join("pkg/etc")).unwrap();
            },
            "pkg",
        );
        std::fs::copy(&evil, root.join("pkg.tar")).unwrap();
        let env = m
            .call(
                "fs_archive",
                json!({ "action": "extract", "src": root.join("pkg.tar").display().to_string(),
                        "dst": root.display().to_string(), "format": "tar" }),
                &ctx(),
            )
            .await;
        assert!(!env.ok, "extracted a symlink: {env:?}");
        assert!(!root.join("pkg").exists());
        assert!(leftovers(&root).is_empty(), "staging left behind");
    }

    #[tokio::test]
    async fn archive_endpoints_are_both_jailed() {
        let (m, root) = setup("archjail");
        let env = m
            .call(
                "fs_archive",
                json!({ "action": "compress", "src": "/etc",
                        "dst": root.join("etc.zip").display().to_string() }),
                &ctx(),
            )
            .await;
        assert!(!env.ok);
        assert_eq!(env.error.unwrap().code, ErrorCode::PolicyDenied);
    }

    // ---- fs_watch ----------------------------------------------------------

    #[tokio::test]
    async fn watch_reports_a_real_change() {
        let (m, root) = setup("watch");
        let target = root.join("watched.txt");
        let t = target.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            std::fs::write(&t, b"new file").unwrap();
        });
        let env = m
            .call(
                "fs_watch",
                json!({ "root": root.display().to_string(), "timeout_ms": 4000 }),
                &ctx(),
            )
            .await;
        assert!(env.ok, "{env:?}");
        let d = env.data.unwrap();
        assert_eq!(d["changed"], true, "{d}");
        assert!(
            d["created"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p.as_str().unwrap().ends_with("watched.txt")),
            "{d}"
        );
    }

    #[tokio::test]
    async fn watch_returns_at_the_timeout_when_nothing_happens() {
        let (m, root) = setup("watchquiet");
        let env = m
            .call(
                "fs_watch",
                json!({ "root": root.display().to_string(), "timeout_ms": 500 }),
                &ctx(),
            )
            .await;
        assert!(env.ok, "{env:?}");
        assert_eq!(env.data.unwrap()["changed"], false);
    }

    // ---- storage / mounts --------------------------------------------------

    #[tokio::test]
    async fn storage_inspect_sees_the_real_boot_disk() {
        let (m, _) = setup("storage");
        let env = m.call("storage_inspect", json!({}), &ctx()).await;
        assert!(env.ok, "{env:?}");
        let d = env.data.unwrap();
        assert!(d["count"].as_u64().unwrap_or(0) > 0, "{d}");
        assert!(
            d["mounted"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["mount"] == "/"),
            "the root filesystem must be listed"
        );
    }

    #[tokio::test]
    async fn mount_list_reads_and_bad_devices_are_refused() {
        let (m, _) = setup("mount");
        let listed = m
            .call("mount_control", json!({ "action": "list" }), &ctx())
            .await;
        assert!(listed.ok, "{listed:?}");
        assert!(listed.data.unwrap()["count"].as_u64().unwrap_or(0) > 0);

        let bad = m
            .call(
                "mount_control",
                json!({ "action": "unmount", "device": "--all" }),
                &ctx(),
            )
            .await;
        assert!(!bad.ok);
        assert_eq!(bad.error.unwrap().code, ErrorCode::InvalidArgs);
    }
}
