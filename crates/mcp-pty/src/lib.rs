//! Interactive terminal sessions.
//!
//! A PTY is a *real shell*, which is exactly the capability `exec`'s argv-only
//! discipline exists to withhold — so `pty_spawn` is gated on the same
//! `terminal.allow_shell` switch, and `pty_write` runs the destructive-command
//! check on everything it sends. Interactive and TUI programs (`vim`, `ssh`,
//! pagers, `htop`) need this; one-shot commands should still use `exec`.

mod ansi;
mod tools;

#[cfg(unix)]
mod session;

pub use ansi::strip as strip_ansi;
pub use tools::{PtyModule, PtyPolicy};

#[cfg(unix)]
pub use session::{PtyError, PtySession};
