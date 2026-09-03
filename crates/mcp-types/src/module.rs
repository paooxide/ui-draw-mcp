use async_trait::async_trait;
use serde_json::Value;

use crate::{CallCtx, Envelope, ToolDescriptor};

/// The single extension surface. Each engine (a11y, input, window, browser, …)
/// implements this: it declares its tool descriptors and handles calls to them.
///
/// An engine never touches transport, policy, or audit directly — those are the
/// core's responsibility. The engine receives already-gated, schema-valid calls
/// and returns an [`Envelope`]. See `docs/architecture.md` §12.
#[async_trait]
pub trait ToolModule: Send + Sync {
    /// Tools this module provides. Called once at registry build time.
    fn descriptors(&self) -> Vec<ToolDescriptor>;

    /// Handle a call to one of this module's tools. `name` is guaranteed to be
    /// one of this module's descriptor names. Semantic argument validation is
    /// the handler's responsibility (map failures to `ErrorCode::InvalidArgs`).
    async fn call(&self, name: &str, args: Value, ctx: &CallCtx) -> Envelope;

    /// Optional per-call risk description, consulted *before* dispatch.
    ///
    /// Returning `Some(summary)` tells the core this specific invocation needs
    /// human approval — e.g. `exec` with a destructive command, or a write that
    /// escapes the configured roots. Engines only *describe* risk here; the core
    /// decides and is the only thing that may ask a human, so an engine (or the
    /// agent driving it) can never approve itself.
    fn consent_prompt(&self, _name: &str, _args: &Value) -> Option<String> {
        None
    }

    /// Release resources the module owns outside this process: child
    /// processes, temporary directories, sessions. Called once when the server
    /// stops serving. Must be synchronous (it runs on shutdown paths where
    /// there may be no runtime left to await on) and idempotent.
    fn shutdown(&self) {}
}
