//! Browser engine: DOM-level control of a Chromium browser over the Chrome
//! DevTools Protocol. Unlike the desktop
//! engines this crate is OS-independent (it speaks CDP over TCP), so the real
//! backend ([`CdpBackend`]) ships here, not in a per-OS crate.

mod backend;
mod cdp;
mod flow;
mod nav;
mod tools;
mod visual;

pub use backend::{BrowserBackend, BrowserError, CdpBackend, Shot, CHROME_BINS};
pub use cdp::DialogPolicy;
pub use flow::{Flow, FlowStore};
pub use nav::{NavDenied, NavPolicy};
pub use tools::BrowserModule;
pub use visual::{Baseline, VisualStore};
