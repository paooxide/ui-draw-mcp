//! Terminal / process engine. OS-independent.

mod exec;
mod tools;

pub use exec::{run, ExecError, ExecOutput, ExecPolicy};
pub use tools::ProcModule;
