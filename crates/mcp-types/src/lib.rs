//! Shared vocabulary for `agentctl-mcp`: the tool result envelope, error codes,
//! tool descriptors (tier/category), the `ToolModule` trait every engine
//! implements, and the per-call context (`CallCtx`) injected into each engine.
//!
//! This crate is the dependency root: engines and the core both depend on it,
//! it depends on nothing in the workspace. See `docs/architecture.md` §4.

mod context;
mod descriptor;
mod envelope;
mod module;

pub use context::{CallCtx, CancelToken, Notifier, Progress};
pub use descriptor::{Category, Tier, ToolDescriptor};
pub use envelope::{Envelope, ErrorCode, ImageContent, ToolError};
pub use module::ToolModule;
