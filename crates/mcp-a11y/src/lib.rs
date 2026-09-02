//! Perception engine: the OS-independent accessibility model, the snapshot
//! arena (element refs + stale detection), and the flattener that turns a UI
//! tree into the compact text an agent reads. Backends (macOS AXUIElement,
//! later UIA/AT-SPI) produce a [`UiNode`] tree; everything downstream is
//! OS-independent and fully testable with fixtures. See `docs/architecture.md`
//! §3 (`mcp-a11y`) and `planning.md` §5.1.

mod arena;
mod backend;
mod flatten;
mod tools;
mod tree;

pub use arena::{ElementInfo, RefError, Snapshot, SnapshotArena};
pub use backend::{A11yBackend, BackendError, RawSnapshot, SnapshotRequest};
pub use flatten::{flatten, FlattenConfig, Flattened};
pub use tools::A11yModule;
pub use tree::{is_interactive_role, normalize_role, Bounds, UiNode};
