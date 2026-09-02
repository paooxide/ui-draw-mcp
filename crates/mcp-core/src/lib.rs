//! MCP protocol + dispatch. Owns JSON-RPC framing (newline-delimited, protocol
//! revision `2025-11-25`), the tool registry, and the single dispatch pipeline
//! that gates every call through `mcp-policy` before an engine runs.
//!
//! `mcp-core` depends on the `ToolModule` trait (via `mcp-types`) and on
//! `mcp-policy`; it does **not** depend on any concrete engine — those are
//! injected at the composition root (`agentctl`). See `docs/architecture.md` §4.

mod http;
mod jsonrpc;
mod registry;
mod server;

pub use http::{generate_token, HttpConfig, HttpTransport};
pub use jsonrpc::{Request, Response, RpcError};
pub use registry::Registry;
pub use server::{Server, DEFAULT_MAX_FRAME_BYTES, PROTOCOL_VERSION};
