//! Network engine. OS-independent.
//!
//! Outbound HTTP is contained by [`mcp_ssrf`]: an agent making requests from this
//! machine sits inside the network perimeter, so the guard is the point.

#[cfg(target_os = "linux")]
mod linux;
mod tools;

/// The guard itself lives in `mcp-ssrf` so the browser engine can share it;
/// re-exported here so callers and the red-team suite keep one import path.
pub use mcp_ssrf::{is_blocked_ip, parse_url, NetPolicy, Parsed, UrlError};
pub use tools::NetModule;
