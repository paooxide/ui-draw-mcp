//! The security kernel. Every tool call passes through [`Policy::gate`] before
//! an engine runs; this crate also owns the denial budget, kill switch, audit
//! sink, and secret redactor. See `docs/architecture.md` §7–§8.
//!
//! `mcp-policy` depends only on `mcp-types` — never on an engine or on
//! `mcp-core`. It gates on descriptor *metadata* (category/tier/name), not on
//! concrete engine types, so it cannot be bypassed or circularly coupled.

mod anonymize;
mod audit;
mod budget;
mod compliance;
mod config;
mod consent;
mod decision;
mod destructive;
mod gate;
mod injection;
mod judged;
mod killswitch;
mod load;
mod redact;
mod role;

pub use anonymize::{
    is_luhn_credit_card, is_token_sink, is_valid_ssn, EntityType, SessionAnonymizer, MAX_ENTITIES,
    TOKEN_SINK_TOOLS,
};
pub use audit::{
    canonical_json, compute_record_hash, generate_signing_key_file, load_signing_key, now_ms,
    parse_audit_records, verify_audit_file, verify_audit_file_pinned, verify_audit_log,
    verify_audit_records, verify_audit_records_pinned, verifying_key_from_hex, AuditRecord,
    AuditSink, AuditTamperError, AuditVerificationReport, GENESIS_PREV_HASH,
};
pub use budget::DenialBudget;
pub use compliance::{
    extract_ephi_tokens, format_utc_timestamp, ComplianceExporter, HipaaAccessEvent,
};
pub use config::{all_categories, default_agentctl_dir, Access, Mode, PolicyConfig};
pub use decision::Decision;
pub use destructive::{default_destructive_patterns, is_destructive};
/// The Ed25519 public key type a verifier pins an audit log to.
pub use ed25519_dalek::VerifyingKey as AuditVerifyingKey;
pub use gate::Policy;
pub use injection::{flag_untrusted, suspicious_instructions};
pub use judged::{judged_destructive, second_opinion_on_content, Destructive};
pub use killswitch::KillSwitch;
pub use mcp_judge;
pub use redact::{redact_flagged_payload, Redactor};
pub use role::{
    check_arguments_invariants, default_protected_paths, is_domain_denied, is_path_protected,
    resolve_role_profile, HardInvariants, RoleProfile,
};

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub use consent::DialogConsent;
pub use consent::{
    applescript_escape, ConsentOutcome, ConsentProvider, ConsentRequest, NoConsent, PromptBudget,
};
pub use load::config_path;
