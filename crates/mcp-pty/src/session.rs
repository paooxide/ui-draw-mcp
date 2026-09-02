//! A real PTY session: `posix_openpt` + a forked child on the slave side.
//!
//! No pty crate. The primitives are a handful of libc calls and `pre_exec`, and
//! the alternative pulls in a dependency for something the standard library
//! plus four ioctls already covers.

use std::io::Write;
use std::os::unix::io::{FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};

#[derive(Debug)]
pub enum PtyError {
    Denied(String),
    NotFound(String),
    Failed(String),
}

fn last_os_error(what: &str) -> PtyError {
    PtyError::Failed(format!("{what}: {}", std::io::Error::last_os_error()))
}

/// One live shell, its master fd, and everything read from it so far.
pub struct PtySession {
    pub id: u64,
    pub shell: String,
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
    master: RawFd,
    child: Child,
    /// Bytes read but not yet handed to the caller.
    pending: Vec<u8>,
    /// True once the buffer cap has discarded output.
    pub truncated: bool,
    max_buffer: usize,
    exit_code: Option<i32>,
}

impl PtySession {
    /// Open a PTY pair and start `shell` on the slave side.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        id: u64,
        shell: &str,
        cwd: &Path,
        cols: u16,
        rows: u16,
        env: &[(String, String)],
        max_buffer: usize,
    ) -> Result<Self, PtyError> {
        unsafe {
            let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
            if master < 0 {
                return Err(last_os_error("posix_openpt"));
            }
            if libc::grantpt(master) < 0 {
                libc::close(master);
                return Err(last_os_error("grantpt"));
            }
            if libc::unlockpt(master) < 0 {
                libc::close(master);
                return Err(last_os_error("unlockpt"));
            }
            let name = libc::ptsname(master);
            if name.is_null() {
                libc::close(master);
                return Err(last_os_error("ptsname"));
            }
            let slave_path = std::ffi::CStr::from_ptr(name)
                .to_string_lossy()
                .into_owned();
            let slave = libc::open(name, libc::O_RDWR | libc::O_NOCTTY);
            if slave < 0 {
                libc::close(master);
                return Err(last_os_error(&format!("open {slave_path}")));
            }

            set_winsize(master, cols, rows)?;

            let mut cmd = Command::new(shell);
            cmd.current_dir(cwd)
                .stdin(Stdio::from_raw_fd_dup(slave)?)
                .stdout(Stdio::from_raw_fd_dup(slave)?)
                .stderr(Stdio::from_raw_fd_dup(slave)?)
                // A scrubbed environment, same as `exec`: inherited variables
                // are a channel for tokens the agent was never handed.
                .env_clear()
                .env("TERM", "dumb")
                .env("LANG", "en_US.UTF-8")
                // Stop the shell reading the operator's rc files, which is both
                // slow and a way for local config to change what runs.
                .env("PS1", "$ ");
            for (k, v) in env {
                cmd.env(k, v);
            }
            let slave_for_child = slave;
            cmd.pre_exec(move || {
                // New session, then claim the pty as the controlling terminal.
                // Without this, job control and Ctrl-C do not work.
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(slave_for_child, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });

            let child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    libc::close(master);
                    libc::close(slave);
                    return Err(if e.kind() == std::io::ErrorKind::NotFound {
                        PtyError::NotFound(format!("shell '{shell}' not found"))
                    } else {
                        PtyError::Failed(format!("spawn {shell}: {e}"))
                    });
                }
            };
            // The parent must drop its slave handle, or the master never sees
            // EOF when the shell exits and `running` stays true forever.
            libc::close(slave);

            let flags = libc::fcntl(master, libc::F_GETFL, 0);
            if flags < 0 || libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                libc::close(master);
                return Err(last_os_error("set O_NONBLOCK"));
            }

            Ok(PtySession {
                id,
                shell: shell.to_string(),
                cwd: cwd.display().to_string(),
                cols,
                rows,
                master,
                child,
                pending: Vec::new(),
                truncated: false,
                max_buffer,
                exit_code: None,
            })
        }
    }

    /// Drain whatever is readable right now. Never blocks.
    pub fn pump(&mut self) {
        let mut buf = [0u8; 8192];
        loop {
            let n = unsafe {
                libc::read(
                    self.master,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                )
            };
            if n > 0 {
                self.pending.extend_from_slice(&buf[..n as usize]);
                if self.pending.len() > self.max_buffer {
                    // Keep the tail: the end of the output is what answers the
                    // question that was asked.
                    let drop = self.pending.len() - self.max_buffer;
                    self.pending.drain(..drop);
                    self.truncated = true;
                }
                continue;
            }
            break;
        }
    }

    /// Everything read so far, ANSI-stripped, clearing the buffer.
    pub fn take_output(&mut self) -> String {
        let raw = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        crate::ansi::strip(&raw)
    }

    pub fn write(&mut self, data: &str) -> Result<usize, PtyError> {
        let mut f = unsafe { std::fs::File::from_raw_fd(dup_fd(self.master)?) };
        let n = f
            .write(data.as_bytes())
            .map_err(|e| PtyError::Failed(format!("pty write: {e}")))?;
        f.flush().ok();
        Ok(n)
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<(), PtyError> {
        set_winsize(self.master, cols, rows)?;
        self.cols = cols;
        self.rows = rows;
        // Tell the foreground program its canvas changed.
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGWINCH);
        }
        Ok(())
    }

    /// Ctrl-C / Ctrl-D go in as *characters* so the line discipline turns them
    /// into a signal for the whole foreground group — signalling the shell's pid
    /// directly would miss whatever it is running.
    pub fn signal(&mut self, kind: &str) -> Result<(), PtyError> {
        match kind {
            "int" => self.write("\x03").map(|_| ()),
            "eof" => self.write("\x04").map(|_| ()),
            "term" => {
                unsafe { libc::kill(-(self.child.id() as i32), libc::SIGTERM) };
                Ok(())
            }
            other => Err(PtyError::Failed(format!("unknown signal '{other}'"))),
        }
    }

    /// Has the shell exited? Caches the code once reaped.
    pub fn running(&mut self) -> bool {
        if self.exit_code.is_some() {
            return false;
        }
        match self.child.try_wait() {
            Ok(Some(status)) => {
                self.exit_code = Some(status.code().unwrap_or(-1));
                false
            }
            Ok(None) => true,
            Err(_) => false,
        }
    }

    pub fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // Kill the whole group: the shell may have children of its own, and
        // leaking them would outlive the session that is supposed to bound them.
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            libc::close(self.master);
        }
        let _ = self.child.wait();
    }
}

fn dup_fd(fd: RawFd) -> Result<RawFd, PtyError> {
    let d = unsafe { libc::dup(fd) };
    if d < 0 {
        return Err(last_os_error("dup"));
    }
    Ok(d)
}

fn set_winsize(fd: RawFd, cols: u16, rows: u16) -> Result<(), PtyError> {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as _, &ws) };
    if rc < 0 {
        return Err(last_os_error("TIOCSWINSZ"));
    }
    Ok(())
}

/// `Stdio` from a duplicate of `fd`, so all three standard streams can share
/// the slave without any of them closing it out from under the others.
trait FromRawFdDup {
    fn from_raw_fd_dup(fd: RawFd) -> Result<Stdio, PtyError>;
}
impl FromRawFdDup for Stdio {
    fn from_raw_fd_dup(fd: RawFd) -> Result<Stdio, PtyError> {
        Ok(unsafe { Stdio::from_raw_fd(dup_fd(fd)?) })
    }
}

/// Read with a deadline: pump until output arrives and then briefly settles, or
/// the budget runs out.
pub async fn read_until_quiet(s: &mut PtySession, timeout_ms: u64, quiet_ms: u64) -> String {
    use tokio::time::{sleep, Duration, Instant};
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut last_len = 0usize;
    let mut quiet_since: Option<Instant> = None;
    loop {
        s.pump();
        let len = s.pending.len();
        if len != last_len {
            last_len = len;
            quiet_since = None;
        } else if len > 0 {
            let since = *quiet_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= Duration::from_millis(quiet_ms) {
                break;
            }
        }
        if Instant::now() >= deadline {
            break;
        }
        sleep(Duration::from_millis(15)).await;
    }
    s.take_output()
}
