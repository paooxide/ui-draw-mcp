//! Optional recall — off unless the `memory`
//! category is enabled.
//!
//! The *agent* records and replays its own verified sequences; the server does
//! no automatic learning and never writes here on its own. Steps are stored as
//! **selectors**, not element refs: a ref belongs to one snapshot, so replaying
//! one points at nothing — or, worse, at a different control.

mod store;
mod tools;

pub use store::{normalize_goal, Recipe, Selector, Step, Store, StoreError};
pub use tools::MemoryModule;
