//! Filesystem engine. OS-independent: everything is
//! `std::fs`, so there is no per-OS backend crate.
//!
//! The security model is containment, not trust: see [`jail`].

mod jail;
#[cfg(target_os = "linux")]
mod linux;
mod tools;

pub use jail::{default_denied, has_traversal, Jail, PathError};
pub use tools::FsModule;
