//! Browser engine: DOM-level control of a Chromium browser over the Chrome
//! DevTools Protocol (`docs/planning.md` §5.11, D11). Unlike the desktop
//! engines this crate is OS-independent — it speaks CDP over TCP — so the real
//! backend ([`CdpBackend`]) ships here, not in a per-OS crate.

mod backend;
mod cdp;
mod tools;

pub use backend::{BrowserBackend, BrowserError, CdpBackend, Shot, CHROME_BINS};
pub use cdp::DialogPolicy;
pub use tools::BrowserModule;
