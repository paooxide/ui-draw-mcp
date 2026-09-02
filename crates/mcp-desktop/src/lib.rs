//! Session, power and desktop settings (`docs/planning.md` §5.10).
//!
//! This is the engine that lets an agent talk *to the human* rather than to the
//! machine: `notify_user` is the only outbound channel that is not a consent
//! dialog. Everything else here reads or nudges the session — volume, appearance,
//! idle state, the media player — plus `power_control`, which is dangerous-tier
//! because ending the session ends every other safeguard along with it.
//!
//! Built on platform CLIs (`osascript`, `pmset`, `ioreg`, `say`, `afplay`)
//! rather than framework bindings, keeping the build lean.

mod backend;
mod tools;

pub use backend::{DesktopBackend, DesktopError, IdleStatus};
pub use tools::DesktopModule;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::MacosDesktop;
