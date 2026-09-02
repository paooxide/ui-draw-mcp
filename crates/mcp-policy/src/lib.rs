//! The security kernel. Every tool call passes through [`Policy::gate`] before
//! an engine runs; this crate also owns the denial budget, kill switch, audit
//! sink, and secret redactor. See `docs/architecture.md` §7–§8.
//!
//! `mcp-policy` depends only on `mcp-types` — never on an engine or on
//! `mcp-core`. It gates on descriptor *metadata* (category/tier/name), not on
//! concrete engine types, so it cannot be bypassed or circularly coupled.

mod audit;
mod budget;
mod config;
mod consent;
mod decision;
mod destructive;
mod gate;
mod killswitch;
mod load;
mod redact;

pub use audit::{now_ms, AuditRecord, AuditSink};
pub use budget::DenialBudget;
pub use config::{default_agentctl_dir, Mode, PolicyConfig};
pub use decision::Decision;
pub use destructive::{default_destructive_patterns, is_destructive};
pub use gate::Policy;
pub use killswitch::KillSwitch;
pub use redact::Redactor;

#[cfg(target_os = "macos")]
pub use consent::DialogConsent;
pub use consent::{
    applescript_escape, ConsentOutcome, ConsentProvider, ConsentRequest, NoConsent, PromptBudget,
};
pub use load::config_path;
