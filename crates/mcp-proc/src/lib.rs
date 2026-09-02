//! Terminal / process engine (`docs/planning.md` §5.4). OS-independent.

mod exec;
mod tools;

pub use exec::{run, ExecError, ExecOutput, ExecPolicy};
pub use tools::ProcModule;
