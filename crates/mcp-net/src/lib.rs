//! Network engine. OS-independent.
//!
//! Outbound HTTP is contained by [`ssrf`]: an agent making requests from this
//! machine sits inside the network perimeter, so the guard is the point.

mod ssrf;
mod tools;

pub use ssrf::{is_blocked_ip, parse_url, NetPolicy, Parsed, UrlError};
pub use tools::NetModule;
