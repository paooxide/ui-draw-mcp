use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::Mutex;

use async_trait::async_trait;
use mcp_types::{CallCtx, Category, Envelope, ErrorCode, Tier, ToolDescriptor, ToolModule};
use serde_json::{json, Value};

#[cfg(unix)]
use crate::session::{read_until_quiet, PtyError, PtySession};

/// What a PTY session is allowed to be.
#[derive(Debug, Clone)]
pub struct PtyPolicy {
    /// Shells that may be started. A PTY hands the agent a real shell, so this
    /// is the list of interpreters an operator is willing to expose.
    pub allowed_shells: Vec<String>,
    /// Mirrors `terminal.allow_shell`. Off means no PTY at all: a shell reached
    /// through a pty is the same capability `exec` withholds by refusing
    /// `sh -c`, and gating one but not the other would be theatre.
    pub allow_shell: bool,
    /// Working directories a session may start in.
    pub roots: Vec<PathBuf>,
    pub max_sessions: usize,
    pub max_buffer: usize,
    pub destructive_patterns: Vec<String>,
    /// No consent channel: destructive input becomes a denial.
    pub autonomous: bool,
    /// Bypass profile: the destructive gate is off entirely.
    pub bypass: bool,
    /// The judge, consulted after the patterns and only able to add a flag.
    pub judge: Option<std::sync::Arc<mcp_policy::mcp_judge::Judge>>,
}

impl Default for PtyPolicy {
    fn default() -> Self {
        PtyPolicy {
            allowed_shells: ["/bin/zsh", "/bin/bash", "/bin/sh"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            allow_shell: false,
            roots: Vec::new(),
            max_sessions: 4,
            max_buffer: 256 * 1024,
            destructive_patterns: mcp_policy::default_destructive_patterns(),
            autonomous: false,
            bypass: false,
            judge: None,
        }
    }
}

pub struct PtyModule {
    policy: PtyPolicy,
    next_id: AtomicU64,
    #[cfg(unix)]
    sessions: Mutex<HashMap<u64, PtySession>>,
    #[cfg(not(unix))]
    sessions: Mutex<HashMap<u64, ()>>,
}

impl PtyModule {
    pub fn new(mut policy: PtyPolicy) -> Self {
        // Canonicalise once, as the filesystem jail does. On macOS `/tmp` is a
        // symlink to `/private/tmp`, so a root configured as `/tmp/work` would
        // never match a resolved path and the engine would refuse its own
        // configured directory.
        policy.roots = policy
            .roots
            .into_iter()
            .map(|r| std::fs::canonicalize(&r).unwrap_or(r))
            .collect();
        PtyModule {
            policy,
            next_id: AtomicU64::new(1),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Sessions currently open.
    pub async fn session_count(&self) -> usize {
        self.sessions.lock().await.len()
    }

    fn resolve_cwd(&self, raw: Option<&str>) -> Result<PathBuf, String> {
        let path = match raw {
            Some(p) => PathBuf::from(p),
            None => self
                .policy
                .roots
                .first()
                .cloned()
                .ok_or_else(|| "no terminal roots configured".to_string())?,
        };
        // Resolve fully, then check: `..` and symlinks must not walk out.
        let real = std::fs::canonicalize(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if !self.policy.roots.iter().any(|r| real.starts_with(r)) {
            return Err(format!(
                "'{}' is outside the configured roots",
                real.display()
            ));
        }
        Ok(real)
    }
}

#[cfg(unix)]
fn pty_err(tool: &str, e: PtyError) -> Envelope {
    match e {
        PtyError::Denied(m) => Envelope::fail(tool, ErrorCode::PolicyDenied, m),
        PtyError::NotFound(m) => Envelope::fail(tool, ErrorCode::NotFound, m),
        PtyError::Failed(m) => Envelope::fail(tool, ErrorCode::ActionFailed, m),
    }
}

/// The shell to start when the caller names none.
///
/// The allowlist is an operator's statement of what may run, not of what is
/// installed: the default list names `/bin/zsh` first, which most Linux boxes
/// lack. So the choice is the user's own `$SHELL` when the list permits it
/// and it exists, else the first allowed shell that exists on disk. When
/// nothing on the list exists the error names every candidate, so the fix is
/// obvious from the message alone.
pub(crate) fn pick_default_shell(
    allowed: &[String],
    env_shell: Option<&str>,
    exists: impl Fn(&str) -> bool,
) -> Result<String, String> {
    if allowed.is_empty() {
        return Err("terminal.allowed_shells is empty, so there is no shell to start".into());
    }
    if let Some(s) = env_shell.filter(|s| !s.is_empty()) {
        if allowed.iter().any(|a| a == s) && exists(s) {
            return Ok(s.to_string());
        }
    }
    if let Some(s) = allowed.iter().find(|a| exists(a)) {
        return Ok(s.clone());
    }
    Err(format!(
        "none of terminal.allowed_shells exists on this machine: {}",
        allowed.join(", ")
    ))
}

fn no_session(tool: &str, id: u64) -> Envelope {
    Envelope::fail_with(
        tool,
        ErrorCode::NotFound,
        format!("no PTY session {id}"),
        "sessions end with pty_close or when the shell exits; start one with pty_spawn",
    )
}

impl PtyModule {
    #[cfg(unix)]
    async fn spawn(&self, args: &Value) -> Envelope {
        let tool = "pty_spawn";
        if !self.policy.allow_shell {
            return Envelope::fail_with(
                tool,
                ErrorCode::PolicyDenied,
                "interactive shells are disabled",
                "a PTY is a real shell: set terminal.allow_shell = \"true\" to permit it, or use \
                 exec for one-shot commands",
            );
        }
        let shell = match args.get("shell").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => match pick_default_shell(
                &self.policy.allowed_shells,
                std::env::var("SHELL").ok().as_deref(),
                |p| std::path::Path::new(p).is_file(),
            ) {
                Ok(s) => s,
                Err(e) => {
                    return Envelope::fail_with(
                        tool,
                        ErrorCode::NotFound,
                        e,
                        "name an installed shell with 'shell', or fix terminal.allowed_shells",
                    )
                }
            },
        };
        if !self.policy.allowed_shells.contains(&shell) {
            return Envelope::fail_with(
                tool,
                ErrorCode::PolicyDenied,
                format!("shell '{shell}' is not in terminal.allowed_shells"),
                "name one of the configured shells by absolute path",
            );
        }
        let cwd = match self.resolve_cwd(args.get("cwd").and_then(Value::as_str)) {
            Ok(p) => p,
            Err(e) => return Envelope::fail(tool, ErrorCode::PolicyDenied, e),
        };
        let cols = args
            .get("cols")
            .and_then(Value::as_u64)
            .unwrap_or(120)
            .clamp(20, 500) as u16;
        let rows = args
            .get("rows")
            .and_then(Value::as_u64)
            .unwrap_or(30)
            .clamp(5, 200) as u16;
        let env: Vec<(String, String)> = args
            .get("env")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        {
            let sessions = self.sessions.lock().await;
            if sessions.len() >= self.policy.max_sessions {
                return Envelope::fail_with(
                    tool,
                    ErrorCode::PolicyDenied,
                    format!("{} sessions already open", sessions.len()),
                    "close one with pty_close",
                );
            }
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut session =
            match PtySession::spawn(id, &shell, &cwd, cols, rows, &env, self.policy.max_buffer) {
                Ok(s) => s,
                Err(e) => return pty_err(tool, e),
            };
        // Let the shell print its first prompt so the caller has something to
        // match against before it writes.
        let banner = read_until_quiet(&mut session, 1200, 150).await;
        self.sessions.lock().await.insert(id, session);
        Envelope::ok(
            tool,
            json!({
                "session_id": id, "shell": shell, "cwd": cwd.display().to_string(),
                "cols": cols, "rows": rows, "output": banner
            }),
        )
    }

    #[cfg(unix)]
    async fn write(&self, args: &Value) -> Envelope {
        let tool = "pty_write";
        let Some(id) = args.get("session_id").and_then(Value::as_u64) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'session_id'");
        };
        let Some(data) = args.get("data").and_then(Value::as_str) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'data'");
        };
        // A PTY write *is* shell input, the one place the destructive gate is
        // unambiguously in scope. The patterns decide first; the judge can
        // only add to what they found. A secret (a password piped to a prompt)
        // skips the remote judge; the offline pattern check still runs. Under
        // bypass the gate is off entirely.
        // Under bypass the destructive gate is off entirely.
        if !self.policy.bypass {
            let secret = args.get("secret").and_then(Value::as_bool).unwrap_or(false);
            let judge = if secret {
                None
            } else {
                self.policy.judge.as_ref()
            };
            let verdict = mcp_policy::judged_destructive(
                data,
                &self.policy.destructive_patterns,
                judge,
                "an interactive shell (pty)",
            )
            .await;
            if verdict.is_destructive() {
                let reason = verdict.reason();
                return if self.policy.autonomous {
                    Envelope::fail(
                        tool,
                        ErrorCode::PolicyDenied,
                        format!("destructive command blocked (autonomous mode): {reason}"),
                    )
                } else {
                    Envelope::fail_with(
                        tool,
                        ErrorCode::ConsentRequired,
                        format!("destructive command requires human consent: {reason}"),
                        "confirm interactively or send a non-destructive command",
                    )
                };
            }
        }
        let timeout = args
            .get("read_timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(2000)
            .clamp(50, 60_000);
        let mut sessions = self.sessions.lock().await;
        let Some(s) = sessions.get_mut(&id) else {
            return no_session(tool, id);
        };
        if let Err(e) = s.write(data) {
            return pty_err(tool, e);
        }
        let output = read_until_quiet(s, timeout, 150).await;
        let running = s.running();
        let truncated = s.truncated;
        Envelope::ok(
            tool,
            json!({ "output": output, "running": running, "truncated": truncated }),
        )
    }

    #[cfg(unix)]
    async fn read(&self, args: &Value) -> Envelope {
        let tool = "pty_read";
        let Some(id) = args.get("session_id").and_then(Value::as_u64) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'session_id'");
        };
        let timeout = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(500)
            .clamp(0, 60_000);
        let mut sessions = self.sessions.lock().await;
        let Some(s) = sessions.get_mut(&id) else {
            return no_session(tool, id);
        };
        let output = read_until_quiet(s, timeout, 100).await;
        let running = s.running();
        Envelope::ok(
            tool,
            json!({
                "output": output, "running": running,
                "truncated": s.truncated, "exit_code": s.exit_code()
            }),
        )
    }

    #[cfg(unix)]
    async fn resize(&self, args: &Value) -> Envelope {
        let tool = "pty_resize";
        let (Some(id), Some(cols), Some(rows)) = (
            args.get("session_id").and_then(Value::as_u64),
            args.get("cols").and_then(Value::as_u64),
            args.get("rows").and_then(Value::as_u64),
        ) else {
            return Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                "need 'session_id', 'cols', 'rows'",
            );
        };
        let mut sessions = self.sessions.lock().await;
        let Some(s) = sessions.get_mut(&id) else {
            return no_session(tool, id);
        };
        match s.resize(cols.clamp(20, 500) as u16, rows.clamp(5, 200) as u16) {
            Ok(()) => Envelope::ok(tool, json!({ "cols": s.cols, "rows": s.rows })),
            Err(e) => pty_err(tool, e),
        }
    }

    #[cfg(unix)]
    async fn signal(&self, args: &Value) -> Envelope {
        let tool = "pty_signal";
        let Some(id) = args.get("session_id").and_then(Value::as_u64) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'session_id'");
        };
        let kind = args.get("signal").and_then(Value::as_str).unwrap_or("int");
        if !matches!(kind, "int" | "term" | "eof") {
            return Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                format!("unknown signal '{kind}' (int|term|eof)"),
            );
        }
        let mut sessions = self.sessions.lock().await;
        let Some(s) = sessions.get_mut(&id) else {
            return no_session(tool, id);
        };
        match s.signal(kind) {
            Ok(()) => Envelope::ok(tool, json!({ "ok": true, "signal": kind })),
            Err(e) => pty_err(tool, e),
        }
    }

    #[cfg(unix)]
    async fn close(&self, args: &Value) -> Envelope {
        let tool = "pty_close";
        let Some(id) = args.get("session_id").and_then(Value::as_u64) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'session_id'");
        };
        let mut sessions = self.sessions.lock().await;
        match sessions.remove(&id) {
            // Drop kills the process group, so nothing outlives the session.
            Some(_) => Envelope::ok(tool, json!({ "closed": true, "session_id": id })),
            None => no_session(tool, id),
        }
    }

    async fn list(&self) -> Envelope {
        let sessions = self.sessions.lock().await;
        #[cfg(unix)]
        let rows: Vec<Value> = sessions
            .values()
            .map(|s| {
                json!({ "session_id": s.id, "shell": s.shell, "cwd": s.cwd,
                             "cols": s.cols, "rows": s.rows })
            })
            .collect();
        #[cfg(not(unix))]
        let rows: Vec<Value> = Vec::new();
        Envelope::ok("pty_list", json!({ "sessions": rows, "count": rows.len() }))
    }
}

#[async_trait]
impl ToolModule for PtyModule {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        let sid = json!({ "type": "integer" });
        vec![
            ToolDescriptor::new(
                "pty_spawn",
                Category::Terminal,
                Tier::Standard,
                "Start an interactive shell in a real PTY, for programs that need a terminal \
                 (vim, ssh, pagers, htop) or for state that must persist across commands. \
                 One-shot commands belong in exec. Requires terminal.allow_shell.",
                json!({"type":"object","properties":{
                    "shell":{"type":"string","description":"absolute path; must be allowlisted"},
                    "cwd":{"type":"string"},"cols":{"type":"integer"},"rows":{"type":"integer"},
                    "env":{"type":"object"}},"required":[]}),
            )
            .untrusted_output(),
            ToolDescriptor::new(
                "pty_write",
                Category::Terminal,
                Tier::Standard,
                "Send input to a session and return what it printed, ANSI-stripped. Include a \
                 trailing newline to submit a command. Destructive commands are gated. Set \
                 'secret' when sending a password to a prompt (e.g. sudo) so it is kept out \
                 of the audit log and never sent to the judge.",
                json!({"type":"object","properties":{
                    "session_id":sid,"data":{"type":"string"},
                    "secret":{"type":"boolean","description":"the data is a password or other secret: keep it out of the audit log and never send it to the judge"},
                    "read_timeout_ms":{"type":"integer"}},"required":["session_id","data"]}),
            )
            .untrusted_output(),
            ToolDescriptor::new(
                "pty_read",
                Category::Terminal,
                Tier::Read,
                "Read pending output without sending anything.",
                json!({"type":"object","properties":{
                    "session_id":sid,"timeout_ms":{"type":"integer"}},"required":["session_id"]}),
            )
            .untrusted_output(),
            ToolDescriptor::new(
                "pty_resize",
                Category::Terminal,
                Tier::Standard,
                "Resize the terminal (TIOCSWINSZ + SIGWINCH) so full-screen programs redraw.",
                json!({"type":"object","properties":{
                    "session_id":sid,"cols":{"type":"integer"},"rows":{"type":"integer"}},
                    "required":["session_id","cols","rows"]}),
            ),
            ToolDescriptor::new(
                "pty_signal",
                Category::Terminal,
                Tier::Standard,
                "Interrupt (Ctrl-C), send EOF (Ctrl-D), or terminate the session's process group.",
                json!({"type":"object","properties":{
                    "session_id":sid,"signal":{"type":"string","enum":["int","term","eof"]}},
                    "required":["session_id"]}),
            ),
            ToolDescriptor::new(
                "pty_close",
                Category::Terminal,
                Tier::Standard,
                "End a session and kill its whole process group.",
                json!({"type":"object","properties":{"session_id":sid},"required":["session_id"]}),
            ),
            ToolDescriptor::new(
                "pty_list",
                Category::Terminal,
                Tier::Read,
                "List open PTY sessions.",
                json!({"type":"object","properties":{},"required":[]}),
            ),
        ]
    }

    /// Destructive shell input asks a human. `pty_write` is the one tool where
    /// the text *is* a command line, so the check is exact rather than heuristic
    /// guessing about which app has focus.
    fn consent_prompt(&self, name: &str, args: &Value) -> Option<String> {
        if name != "pty_write" {
            return None;
        }
        let data = args.get("data").and_then(Value::as_str)?;
        mcp_policy::is_destructive(data, &self.policy.destructive_patterns)
            .then(|| format!("Run this in the shell?\n\n{}", data.trim()))
    }

    async fn call(&self, name: &str, args: Value, _ctx: &CallCtx) -> Envelope {
        #[cfg(unix)]
        {
            match name {
                "pty_spawn" => self.spawn(&args).await,
                "pty_write" => self.write(&args).await,
                "pty_read" => self.read(&args).await,
                "pty_resize" => self.resize(&args).await,
                "pty_signal" => self.signal(&args).await,
                "pty_close" => self.close(&args).await,
                "pty_list" => self.list().await,
                other => Envelope::fail(other, ErrorCode::InvalidArgs, "unknown tool"),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (&args, self.list().await);
            Envelope::fail(
                name,
                ErrorCode::UnsupportedOs,
                "PTY sessions need a Unix pty; not available on this platform",
            )
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use mcp_types::CancelToken;

    fn ctx() -> CallCtx {
        CallCtx::new("t", CancelToken::new())
    }

    fn open_policy() -> PtyPolicy {
        PtyPolicy {
            allow_shell: true,
            roots: vec![std::env::temp_dir().canonicalize().unwrap()],
            ..PtyPolicy::default()
        }
    }

    async fn spawn(m: &PtyModule) -> u64 {
        let env = m
            .call("pty_spawn", json!({ "shell": "/bin/sh" }), &ctx())
            .await;
        assert!(env.ok, "{env:?}");
        env.data.unwrap()["session_id"].as_u64().unwrap()
    }

    /// A PTY is a real shell, which is exactly what `exec`'s argv discipline
    /// withholds. Gating one but not the other would be theatre.
    #[tokio::test]
    async fn a_pty_is_refused_unless_shells_are_allowed() {
        let m = PtyModule::new(PtyPolicy::default());
        let env = m.call("pty_spawn", json!({}), &ctx()).await;
        assert!(!env.ok);
        let e = env.error.unwrap();
        assert_eq!(e.code, ErrorCode::PolicyDenied);
        assert!(e.suggestion.unwrap().contains("allow_shell"));
    }

    #[tokio::test]
    async fn only_allowlisted_shells_start() {
        let m = PtyModule::new(open_policy());
        let env = m
            .call("pty_spawn", json!({ "shell": "/usr/bin/python3" }), &ctx())
            .await;
        assert!(!env.ok);
        assert_eq!(env.error.unwrap().code, ErrorCode::PolicyDenied);
    }

    /// The session is stateful: that is the whole reason it exists. `cd` in one
    /// write must be visible to the next.
    #[tokio::test]
    async fn state_persists_across_writes() {
        let m = PtyModule::new(open_policy());
        let id = spawn(&m).await;
        let out = m
            .call(
                "pty_write",
                json!({ "session_id": id, "data": "cd /usr && echo MARK-$PWD\n" }),
                &ctx(),
            )
            .await;
        assert!(out.ok, "{out:?}");
        let d = m
            .call(
                "pty_write",
                json!({ "session_id": id, "data": "echo SECOND-$PWD\n" }),
                &ctx(),
            )
            .await
            .data
            .unwrap();
        let text = d["output"].as_str().unwrap();
        assert!(text.contains("SECOND-/usr"), "cd did not persist: {text:?}");
        assert_eq!(d["running"], true);
        m.call("pty_close", json!({ "session_id": id }), &ctx())
            .await;
    }

    /// Output must arrive as text, not as a rendering protocol.
    #[tokio::test]
    async fn output_is_ansi_stripped() {
        let m = PtyModule::new(open_policy());
        let id = spawn(&m).await;
        let d = m
            .call(
                "pty_write",
                json!({ "session_id": id, "data": "printf '\\033[31mRED\\033[0m\\n'\n" }),
                &ctx(),
            )
            .await
            .data
            .unwrap();
        let text = d["output"].as_str().unwrap();
        assert!(text.contains("RED"), "{text:?}");
        assert!(!text.contains('\u{1b}'), "escape bytes leaked: {text:?}");
        m.call("pty_close", json!({ "session_id": id }), &ctx())
            .await;
    }

    #[tokio::test]
    async fn destructive_input_is_gated_and_never_reaches_the_shell() {
        let m = PtyModule::new(PtyPolicy {
            autonomous: true,
            ..open_policy()
        });
        let id = spawn(&m).await;
        let env = m
            .call(
                "pty_write",
                json!({ "session_id": id, "data": "rm -rf /tmp/should-not-happen\n" }),
                &ctx(),
            )
            .await;
        assert!(!env.ok);
        assert_eq!(env.error.unwrap().code, ErrorCode::PolicyDenied);

        // And the interactive path asks rather than refuses.
        let interactive = PtyModule::new(open_policy());
        assert!(interactive
            .consent_prompt("pty_write", &json!({ "data": "sudo rm -rf /" }))
            .is_some());
        assert!(interactive
            .consent_prompt("pty_write", &json!({ "data": "ls -la" }))
            .is_none());
        m.call("pty_close", json!({ "session_id": id }), &ctx())
            .await;
    }

    #[tokio::test]
    async fn cwd_outside_the_roots_is_refused() {
        let m = PtyModule::new(open_policy());
        let env = m.call("pty_spawn", json!({ "cwd": "/etc" }), &ctx()).await;
        assert!(!env.ok);
        assert_eq!(env.error.unwrap().code, ErrorCode::PolicyDenied);
    }

    /// The family: an allowlist whose first entry is not installed. The
    /// default must be the first entry that is, and `$SHELL` wins when the
    /// list permits it.
    #[test]
    fn default_shell_is_the_first_allowed_one_that_exists() {
        let allowed: Vec<String> = ["/bin/zsh", "/bin/bash", "/bin/sh"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let on_disk = |p: &str| matches!(p, "/bin/bash" | "/bin/sh" | "/usr/bin/fish");
        assert_eq!(
            pick_default_shell(&allowed, None, on_disk).unwrap(),
            "/bin/bash",
            "first entry missing, second present"
        );
        assert_eq!(
            pick_default_shell(&allowed, Some("/bin/sh"), on_disk).unwrap(),
            "/bin/sh",
            "$SHELL is allowed and present, so it wins"
        );
        assert_eq!(
            pick_default_shell(&allowed, Some("/usr/bin/fish"), on_disk).unwrap(),
            "/bin/bash",
            "$SHELL outside the allowlist is ignored"
        );
        assert_eq!(
            pick_default_shell(&allowed, Some("/bin/zsh"), on_disk).unwrap(),
            "/bin/bash",
            "$SHELL allowed but missing is ignored"
        );
        assert_eq!(
            pick_default_shell(&allowed, Some(""), on_disk).unwrap(),
            "/bin/bash",
            "an empty $SHELL is no $SHELL"
        );
        let all_present = |_: &str| true;
        assert_eq!(
            pick_default_shell(&allowed, None, all_present).unwrap(),
            "/bin/zsh",
            "when everything exists the first entry is the default, as before"
        );
    }

    #[test]
    fn no_installed_shell_names_every_candidate() {
        let allowed: Vec<String> = vec!["/bin/zsh".into(), "/opt/fish".into()];
        let err = pick_default_shell(&allowed, Some("/bin/zsh"), |_| false).unwrap_err();
        assert!(err.contains("/bin/zsh"), "{err}");
        assert!(err.contains("/opt/fish"), "{err}");
        assert!(err.contains("allowed_shells"), "{err}");
        let err = pick_default_shell(&[], None, |_| true).unwrap_err();
        assert!(err.contains("empty"), "{err}");
    }

    /// Against the real disk: `/bin/sh` exists everywhere, a made-up path
    /// does not, and the spawned session reports which one it got.
    #[tokio::test]
    async fn spawn_without_a_shell_uses_one_that_exists() {
        let m = PtyModule::new(PtyPolicy {
            allowed_shells: vec!["/nonexistent/zsh".into(), "/bin/sh".into()],
            ..open_policy()
        });
        let env = m.call("pty_spawn", json!({}), &ctx()).await;
        assert!(env.ok, "{env:?}");
        assert_eq!(env.data.unwrap()["shell"], "/bin/sh");
        assert_eq!(m.session_count().await, 1);

        let m = PtyModule::new(PtyPolicy {
            allowed_shells: vec!["/nonexistent/zsh".into(), "/nonexistent/fish".into()],
            ..open_policy()
        });
        let env = m.call("pty_spawn", json!({}), &ctx()).await;
        assert!(!env.ok);
        let e = env.error.unwrap();
        assert_eq!(e.code, ErrorCode::NotFound);
        assert!(e.message.contains("/nonexistent/zsh"), "{}", e.message);
        assert!(e.message.contains("/nonexistent/fish"), "{}", e.message);
        assert_eq!(m.session_count().await, 0, "a failed spawn holds no slot");
    }

    #[tokio::test]
    async fn sessions_are_capped_and_close_frees_a_slot() {
        let m = PtyModule::new(PtyPolicy {
            max_sessions: 1,
            ..open_policy()
        });
        let id = spawn(&m).await;
        let second = m.call("pty_spawn", json!({}), &ctx()).await;
        assert!(!second.ok, "cap must hold");
        m.call("pty_close", json!({ "session_id": id }), &ctx())
            .await;
        assert_eq!(m.session_count().await, 0);
        assert!(
            m.call("pty_spawn", json!({}), &ctx()).await.ok,
            "slot freed"
        );
    }

    #[tokio::test]
    async fn unknown_session_is_not_found_with_a_way_forward() {
        let m = PtyModule::new(open_policy());
        for tool in ["pty_write", "pty_read", "pty_close", "pty_signal"] {
            let env = m
                .call(tool, json!({ "session_id": 9999, "data": "x" }), &ctx())
                .await;
            assert!(!env.ok, "{tool}");
            let e = env.error.unwrap();
            assert_eq!(e.code, ErrorCode::NotFound, "{tool}");
            assert!(e.suggestion.is_some(), "{tool}");
        }
    }

    /// Exiting the shell must be observable, and the session must not linger.
    #[tokio::test]
    async fn shell_exit_is_reported() {
        let m = PtyModule::new(open_policy());
        let id = spawn(&m).await;
        m.call(
            "pty_write",
            json!({ "session_id": id, "data": "exit\n" }),
            &ctx(),
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let d = m
            .call(
                "pty_read",
                json!({ "session_id": id, "timeout_ms": 200 }),
                &ctx(),
            )
            .await
            .data
            .unwrap();
        assert_eq!(
            d["running"], false,
            "exited shell must report running=false"
        );
        m.call("pty_close", json!({ "session_id": id }), &ctx())
            .await;
    }

    #[tokio::test]
    async fn resize_and_signal_reach_the_session() {
        let m = PtyModule::new(open_policy());
        let id = spawn(&m).await;
        let d = m
            .call(
                "pty_resize",
                json!({ "session_id": id, "cols": 100, "rows": 40 }),
                &ctx(),
            )
            .await;
        assert!(d.ok, "{d:?}");
        assert_eq!(d.data.unwrap()["cols"], 100);
        assert!(
            m.call(
                "pty_signal",
                json!({ "session_id": id, "signal": "int" }),
                &ctx()
            )
            .await
            .ok
        );
        assert!(
            !m.call(
                "pty_signal",
                json!({ "session_id": id, "signal": "nope" }),
                &ctx()
            )
            .await
            .ok
        );
        let listed = m.call("pty_list", json!({}), &ctx()).await.data.unwrap();
        assert_eq!(listed["count"], 1);
        m.call("pty_close", json!({ "session_id": id }), &ctx())
            .await;
    }
}

#[cfg(all(test, unix))]
mod root_tests {
    use super::*;
    use mcp_types::CancelToken;

    /// `/tmp` is a symlink to `/private/tmp` on macOS. A root configured with
    /// the un-resolved name must still match, or the engine refuses the very
    /// directory it was pointed at.
    #[tokio::test]
    async fn a_symlinked_root_still_matches_its_resolved_form() {
        let raw = std::path::PathBuf::from("/tmp");
        let real = std::fs::canonicalize(&raw).unwrap();
        if raw == real {
            return; // no symlink on this platform; nothing to prove
        }
        let m = PtyModule::new(PtyPolicy {
            allow_shell: true,
            roots: vec![raw],
            ..PtyPolicy::default()
        });
        let env = m
            .call(
                "pty_spawn",
                json!({ "shell": "/bin/sh", "cwd": real.display().to_string() }),
                &CallCtx::new("t", CancelToken::new()),
            )
            .await;
        assert!(env.ok, "configured root was refused: {env:?}");
        let id = env.data.unwrap()["session_id"].as_u64().unwrap();
        m.call(
            "pty_close",
            json!({ "session_id": id }),
            &CallCtx::new("t", CancelToken::new()),
        )
        .await;
    }
}
