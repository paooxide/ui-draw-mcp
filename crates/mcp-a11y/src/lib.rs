//! Perception engine: the OS-independent accessibility model, the snapshot
//! arena (element refs + stale detection), and the flattener that turns a UI
//! tree into the compact text an agent reads. Backends (macOS AXUIElement,
//! later UIA/AT-SPI) produce a [`UiNode`] tree; everything downstream is
//! OS-independent and fully testable with fixtures. See `docs/architecture.md`
//! §3 (`mcp-a11y`).

mod arena;
mod backend;
mod diff;
mod extract;
mod flatten;
mod query;
mod tools;
mod tree;

pub use arena::{ElementInfo, ElementState, RefError, Snapshot, SnapshotArena};
pub use backend::{A11yBackend, BackendError, RawSnapshot, SnapshotRequest};
pub use diff::{diff_snapshots, DeltaEntry, SnapshotDelta};
pub use extract::extract_data;
pub use flatten::{flatten, FlattenConfig, Flattened};
pub use query::{parse_query, query_schema, query_snapshot, ElementHit, ElementQuery};
pub use tools::A11yModule;
pub use tree::{is_interactive_role, normalize_role, Bounds, UiNode};
