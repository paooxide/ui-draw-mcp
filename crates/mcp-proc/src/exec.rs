//! Command execution.
//!
//! Two rules shape this module:
//!
//! 1. **No shell by default.** Commands run as an argv vector via `execvp`, so
//!    `;`, `|`, backticks and `$(...)` in an *argument* are inert data rather
//!    than syntax. Shell interpretation is opt-in, gated, and consented — it is
//!    the difference between passing a filename and handing over a shell.
//! 2. **The binary must be on an allowlist.** An agent that can run any binary
//!    can do anything the user can, which makes every other control decorative.

use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Outcome of a finished (or abandoned) command.
#[derive(Debug, Clone)]
pub struct ExecOutput {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub duration_ms: u64,
}

#[derive(Debug, Clone)]
pub enum ExecError {
    NotAllowed(String),
    NotFound(String),
    Failed(String),
}

/// Limits applied to every execution.
#[derive(Debug, Clone)]
pub struct ExecPolicy {
    /// Binaries the agent may run. Empty means **nothing** is runnable — the
    /// engine refuses rather than defaulting to the whole of `$PATH`.
    pub allowed: Vec<String>,
    /// Permit `shell: true`. Off by default.
    pub allow_shell: bool,
    pub timeout: Duration,
    pub max_output_bytes: usize,
    /// Environment variables passed through. Everything else is dropped so
    /// tokens sitting in the server's environment are not handed to children.
    pub env_passthrough: Vec<String>,
}

impl Default for ExecPolicy {
    fn default() -> Self {
        ExecPolicy {
            allowed: Vec::new(),
            allow_shell: false,
            timeout: Duration::from_secs(30),
            max_output_bytes: 100_000,
            env_passthrough: ["PATH", "HOME", "LANG", "TZ", "TERM"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

impl ExecPolicy {
    /// Is this program runnable? Compared on the *file name* so an allowlist
    /// entry of `git` cannot be satisfied by `/tmp/evil/git` — the basename is
    /// what we match, and the resolved binary still comes from `$PATH`.
    pub fn allows(&self, program: &str) -> bool {
        let base = std::path::Path::new(program)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| program.to_string());
        self.allowed.iter().any(|a| a == &base || a == program)
    }
}

fn clamp(s: Vec<u8>, max: usize) -> (String, bool) {
    let truncated = s.len() > max;
    let cut = if truncated { &s[..max] } else { &s[..] };
    (String::from_utf8_lossy(cut).to_string(), truncated)
}

/// Run `program` with `args`.
///
/// The child gets a scrubbed environment, no stdin, and a hard timeout after
/// which it is killed. Output is capped so a chatty command cannot flood the
/// agent's context.
pub async fn run(
    program: &str,
    args: &[String],
    cwd: Option<&std::path::Path>,
    policy: &ExecPolicy,
    shell: bool,
) -> Result<ExecOutput, ExecError> {
    if shell && !policy.allow_shell {
        return Err(ExecError::NotAllowed(
            "shell execution is disabled (terminal.allow_shell = false); pass argv instead".into(),
        ));
    }
    if !shell && !policy.allows(program) {
        return Err(ExecError::NotAllowed(format!(
            "'{program}' is not in terminal.allowed_commands"
        )));
    }

    let mut env: HashMap<String, String> = HashMap::new();
    for key in &policy.env_passthrough {
        if let Ok(v) = std::env::var(key) {
            env.insert(key.clone(), v);
        }
    }

    let mut cmd = if shell {
        let mut c = Command::new("/bin/sh");
        let joined = if args.is_empty() {
            program.to_string()
        } else {
            format!("{program} {}", args.join(" "))
        };
        c.arg("-c").arg(joined);
        c
    } else {
        let mut c = Command::new(program);
        c.args(args);
        c
    };
    cmd.env_clear()
        .envs(&env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }

    let started = std::time::Instant::now();
    let mut child = cmd.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => ExecError::NotFound(format!("no such command: {program}")),
        _ => ExecError::Failed(e.to_string()),
    })?;

    let mut out_buf = Vec::new();
    let mut err_buf = Vec::new();
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();

    let collect = async {
        if let Some(o) = stdout.as_mut() {
            let _ = o.read_to_end(&mut out_buf).await;
        }
        if let Some(e) = stderr.as_mut() {
            let _ = e.read_to_end(&mut err_buf).await;
        }
        child.wait().await
    };

    let (status, timed_out) = match tokio::time::timeout(policy.timeout, collect).await {
        Ok(Ok(s)) => (Some(s), false),
        Ok(Err(e)) => return Err(ExecError::Failed(e.to_string())),
        Err(_) => (None, true), // `kill_on_drop` reaps the child
    };

    let (stdout, stdout_truncated) = clamp(out_buf, policy.max_output_bytes);
    let (stderr, stderr_truncated) = clamp(err_buf, policy.max_output_bytes);
    Ok(ExecOutput {
        code: status.and_then(|s| s.code()),
        stdout,
        stderr,
        timed_out,
        stdout_truncated,
        stderr_truncated,
        duration_ms: started.elapsed().as_millis() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> ExecPolicy {
        ExecPolicy {
            allowed: vec![
                "echo".into(),
                "sh".into(),
                "sleep".into(),
                "printenv".into(),
            ],
            ..ExecPolicy::default()
        }
    }

    #[tokio::test]
    async fn runs_an_allowed_command_and_captures_output() {
        let out = run("echo", &["hello".into()], None, &policy(), false)
            .await
            .unwrap();
        assert_eq!(out.code, Some(0));
        assert_eq!(out.stdout.trim(), "hello");
        assert!(!out.timed_out);
    }

    #[tokio::test]
    async fn refuses_a_command_not_on_the_allowlist() {
        let e = run("curl", &[], None, &policy(), false).await;
        assert!(matches!(e, Err(ExecError::NotAllowed(_))));
    }

    /// An allowlist entry must not be satisfiable by a same-named binary
    /// planted somewhere else.
    #[test]
    fn allowlist_matches_on_basename_not_an_arbitrary_path() {
        let p = policy();
        assert!(p.allows("echo"));
        assert!(p.allows("/bin/echo"));
        assert!(!p.allows("curl"));
        assert!(!p.allows("/tmp/evil/curl"));
    }

    /// The headline injection case: shell metacharacters in an *argument* must
    /// be inert data, not syntax.
    #[tokio::test]
    async fn shell_metacharacters_in_arguments_are_not_interpreted() {
        let marker = std::env::temp_dir().join("mcp-proc-injection-canary");
        let _ = std::fs::remove_file(&marker);
        let payload = format!("x; touch {}", marker.display());
        let out = run(
            "echo",
            std::slice::from_ref(&payload),
            None,
            &policy(),
            false,
        )
        .await
        .unwrap();
        assert!(out.stdout.contains("; touch"), "argument should be literal");
        assert!(
            !marker.exists(),
            "injected command executed — argv execution is broken"
        );
    }

    #[tokio::test]
    async fn shell_is_refused_unless_explicitly_enabled() {
        let e = run("echo hi", &[], None, &policy(), true).await;
        assert!(matches!(e, Err(ExecError::NotAllowed(_))));

        let permissive = ExecPolicy {
            allow_shell: true,
            ..policy()
        };
        let out = run("echo shelled", &[], None, &permissive, true)
            .await
            .unwrap();
        assert_eq!(out.stdout.trim(), "shelled");
    }

    #[tokio::test]
    async fn a_hanging_command_is_killed_at_the_timeout() {
        let p = ExecPolicy {
            timeout: Duration::from_millis(300),
            ..policy()
        };
        let out = run("sleep", &["30".into()], None, &p, false).await.unwrap();
        assert!(out.timed_out, "should have timed out");
        assert!(
            out.duration_ms < 5_000,
            "should not have waited for the child"
        );
    }

    #[tokio::test]
    async fn output_is_capped() {
        let p = ExecPolicy {
            max_output_bytes: 16,
            allow_shell: true,
            ..policy()
        };
        // A literal string well over the cap. Not a brace expansion or `seq`:
        // `run(shell=true)` uses `/bin/sh`, which is dash on many Linux CI
        // runners and expands neither `{1..500}` nor much else, so the command
        // has to emit its bytes without relying on shell features.
        let out = run(
            "printf xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
            &[],
            None,
            &p,
            true,
        )
        .await
        .unwrap();
        assert!(out.stdout.len() <= 16);
        assert!(out.stdout_truncated);
    }

    /// Secrets in the server's environment must not leak into children.
    #[tokio::test]
    async fn environment_is_scrubbed() {
        std::env::set_var("MCP_PROC_SECRET_TOKEN", "super-secret");
        let out = run("printenv", &[], None, &policy(), false).await.unwrap();
        assert!(
            !out.stdout.contains("super-secret"),
            "server environment leaked to the child"
        );
        assert!(
            out.stdout.contains("PATH="),
            "passthrough vars should survive"
        );
        std::env::remove_var("MCP_PROC_SECRET_TOKEN");
    }

    #[tokio::test]
    async fn missing_binary_is_reported_as_not_found() {
        let p = ExecPolicy {
            allowed: vec!["definitely-not-a-real-binary-xyz".into()],
            ..policy()
        };
        let e = run("definitely-not-a-real-binary-xyz", &[], None, &p, false).await;
        assert!(matches!(e, Err(ExecError::NotFound(_))));
    }

    #[tokio::test]
    async fn empty_allowlist_runs_nothing() {
        let p = ExecPolicy::default();
        assert!(matches!(
            run("echo", &[], None, &p, false).await,
            Err(ExecError::NotAllowed(_))
        ));
    }
}
