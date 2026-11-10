//! `agentctl` as a library: the composition root's reusable half.
//!
//! The binary is a thin CLI over this. Tests, the tool-reference generator and
//! the reference-client bridge all build the server through [`build_server`],
//! so what they exercise is the real wiring rather than a copy of it.

use std::sync::Arc;

use mcp_core::{Registry, Server};
use mcp_policy::{AuditSink, Policy, PolicyConfig, Redactor};

pub mod bridge;
pub mod engines;
pub mod override_watch;
pub mod tools_doc;
pub mod tools_system;
pub mod transcript;

pub use engines::{
    build_modules, build_stack, demo_showcase, showcase_speed, EngineConfig, Wiring,
};

/// The out-of-band human-approval channel.
///
/// On macOS this is a real native dialog and on Linux a zenity dialog (or a
/// notification with buttons), both with **Deny** as the default and a timeout
/// that denies. Elsewhere there is no channel yet, so consent requests are
/// refused rather than silently allowed.
pub fn consent_provider() -> Arc<dyn mcp_policy::ConsentProvider> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        Arc::new(mcp_policy::DialogConsent::default())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
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
    let audit = if let Some(r) = &cfg.active_role {
        audit.with_role(r)
    } else {
        audit
    };
    let mut policy = Policy::new(cfg, audit, Redactor::empty()).with_consent(consent);
    if let Some(judge) = wiring.judge.clone() {
        policy = policy.with_judge(judge);
    }
    let policy = Arc::new(policy);
    let session_id = session_id.into();
    bind_input_steps(&wiring, &policy, &session_id)?;
    Ok((
        Server::new(registry, policy.clone(), session_id),
        policy,
        wiring,
    ))
}

/// Give `input_steps` its way back through the gate.
///
/// A step must meet every check a call on its own meets, and the only code
/// that applies them all (tier, role allow and deny lists, consent, hard
/// invariants, kill switch, denial budget, audit) is `Server::dispatch_call`.
/// So the batch is handed a second server over the same input engine, the same
/// policy and the same audit log, and each step is a call to it. It is not a
/// second policy: nothing here decides anything, and a role, a mode or a
/// kill switch changed on the first server's policy is the one this server
/// asks. The engine's `shutdown` lets go of it, which breaks the cycle
/// (server, engine, handle, server).
pub fn bind_input_steps(
    wiring: &Wiring,
    policy: &Arc<Policy>,
    session_id: &str,
) -> Result<(), String> {
    let Some((input, relay)) = &wiring.steps else {
        return Ok(());
    };
    let registry = Registry::build(vec![input.clone()])?;
    let server = Server::new(registry, policy.clone(), session_id);
    relay.bind(Arc::new(GatedSteps(server)));
    Ok(())
}

/// The steps of a batch, run through a server's `dispatch_call`.
struct GatedSteps(Server);

#[async_trait::async_trait]
impl mcp_input::StepDispatch for GatedSteps {
    async fn dispatch(&self, tool: &str, args: serde_json::Value) -> mcp_types::Envelope {
        self.0.dispatch_call(tool, args).await
    }

    fn cancel(&self) {
        self.0.cancel_all();
    }
}
