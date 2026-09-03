//! `agentctl` as a library: the composition root's reusable half.
//!
//! The binary is a thin CLI over this. Tests, the tool-reference generator and
//! the reference-client bridge all build the server through [`build_server`],
//! so what they exercise is the real wiring rather than a copy of it.

use std::sync::Arc;

use mcp_core::{Registry, Server};
use mcp_policy::{AuditSink, Policy, PolicyConfig, Redactor};

pub mod engines;
pub mod override_watch;
pub mod tools_doc;
pub mod tools_system;

pub use engines::{build_modules, build_stack, EngineConfig, Wiring};

/// The out-of-band human-approval channel.
///
/// On macOS this is a real native dialog (default button **Deny**, and the
/// timeout denies). Elsewhere there is no channel yet, so consent requests are
/// refused rather than silently allowed.
pub fn consent_provider() -> Arc<dyn mcp_policy::ConsentProvider> {
    #[cfg(target_os = "macos")]
    {
        Arc::new(mcp_policy::DialogConsent::default())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Arc::new(mcp_policy::NoConsent)
    }
}

/// A session id that identifies this run in the audit log.
pub fn new_session_id() -> String {
    format!("sess-{}-{}", std::process::id(), mcp_policy::now_ms())
}

/// Build the whole server: engines wired per config, behind the policy gate.
///
/// One constructor for the binary and for every test, so a test cannot
/// accidentally exercise a registry the server would never serve.
pub fn build_server(
    cfg: PolicyConfig,
    audit: AuditSink,
    consent: Arc<dyn mcp_policy::ConsentProvider>,
    session_id: impl Into<String>,
) -> Result<Server, String> {
    Ok(build_server_with(cfg, audit, consent, session_id)?.0)
}

/// As [`build_server`], but also returning the handles the human-override
/// watcher needs. Tests use the simpler form; only `serve` starts a watcher.
pub fn build_server_with(
    cfg: PolicyConfig,
    audit: AuditSink,
    consent: Arc<dyn mcp_policy::ConsentProvider>,
    session_id: impl Into<String>,
) -> Result<(Server, Arc<Policy>, engines::Wiring), String> {
    let (modules, wiring) = build_stack(&cfg);
    let registry = Registry::build(modules)?;
    let policy = Arc::new(Policy::new(cfg, audit, Redactor::empty()).with_consent(consent));
    let session_id = session_id.into();
    Ok((
        Server::new(registry, policy.clone(), session_id),
        policy,
        wiring,
    ))
}
