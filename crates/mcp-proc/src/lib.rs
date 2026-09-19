//! Terminal / process engine. OS-independent.

mod exec;
#[cfg(target_os = "linux")]
mod linux;
mod tools;

pub use exec::{run, ExecError, ExecOutput, ExecPolicy};
pub use tools::ProcModule;
