use std::collections::HashMap;

use serde::Serialize;

use crate::tree::Bounds;

/// What we retain per element ref: enough for `get_element`, and for memory to
/// convert a ref into a `{role, name}` selector. Never stores a live native
/// handle across snapshots (refs are re-resolved).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ElementInfo {
    pub role: String,
    pub name: Option<String>,
    pub value_preview: Option<String>,
    pub secure: bool,
    pub bounds: Option<Bounds>,
    /// Backend token to act on this element (input tools). `None` if the backend
    /// did not provide one.
    pub node_id: Option<u64>,
}

/// One captured snapshot: an id, the app/window it came from, and the `@eN` ->
/// info map.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub id: String,
    pub app: Option<String>,
    pub window: Option<String>,
    pub elements: HashMap<String, ElementInfo>,
}

/// Why a ref failed to resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefError {
    /// The ref belongs to a superseded snapshot.
    Stale,
    /// The ref does not exist in the current snapshot.
    NotFound,
}

/// Holds the latest snapshot. Installing a new one supersedes the old, so its
/// refs become [`RefError::Stale`]. Concurrency wrapping (`Arc<RwLock<_>>`) is
/// added when this is wired into `CallCtx`; the logic here is single-owner.
#[derive(Default)]
pub struct SnapshotArena {
    current: Option<Snapshot>,
}

impl SnapshotArena {
    pub fn new() -> Self {
        SnapshotArena::default()
    }

    /// Install a new snapshot, superseding any previous one.
    pub fn install(&mut self, snapshot: Snapshot) {
        self.current = Some(snapshot);
    }

    pub fn current_id(&self) -> Option<&str> {
        self.current.as_ref().map(|s| s.id.as_str())
    }

    pub fn current(&self) -> Option<&Snapshot> {
        self.current.as_ref()
    }

    /// Resolve `reff` (`@eN`) scoped to `snapshot_id`. Fails `Stale` if the id
    /// is not the current snapshot, `NotFound` if the ref is absent.
    pub fn resolve(&self, snapshot_id: &str, reff: &str) -> Result<&ElementInfo, RefError> {
        let snap = self.current.as_ref().ok_or(RefError::Stale)?;
        if snap.id != snapshot_id {
            return Err(RefError::Stale);
        }
        snap.elements.get(reff).ok_or(RefError::NotFound)
    }

    /// Resolve `reff` against whatever the current snapshot is (the server
    /// tracks "latest"; granular clients don't pass a snapshot id). Fails
    /// `Stale` if no snapshot has been taken yet.
    pub fn resolve_latest(&self, reff: &str) -> Result<&ElementInfo, RefError> {
        let snap = self.current.as_ref().ok_or(RefError::Stale)?;
        snap.elements.get(reff).ok_or(RefError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(role: &str) -> ElementInfo {
        ElementInfo {
            role: role.into(),
            name: None,
            value_preview: None,
            secure: false,
            bounds: None,
            node_id: None,
        }
    }

    fn snap(id: &str) -> Snapshot {
        let mut elements = HashMap::new();
        elements.insert("@e1".to_string(), info("button"));
        Snapshot {
            id: id.into(),
            app: None,
            window: None,
            elements,
        }
    }

    #[test]
    fn resolves_current_ref() {
        let mut a = SnapshotArena::new();
        a.install(snap("s1"));
        assert_eq!(a.resolve("s1", "@e1").unwrap().role, "button");
    }

    #[test]
    fn stale_after_new_snapshot() {
        let mut a = SnapshotArena::new();
        a.install(snap("s1"));
        a.install(snap("s2"));
        assert_eq!(a.resolve("s1", "@e1"), Err(RefError::Stale));
        assert!(a.resolve("s2", "@e1").is_ok());
    }

    #[test]
    fn unknown_ref_is_not_found() {
        let mut a = SnapshotArena::new();
        a.install(snap("s1"));
        assert_eq!(a.resolve("s1", "@e9"), Err(RefError::NotFound));
    }

    #[test]
    fn empty_arena_is_stale() {
        let a = SnapshotArena::new();
        assert_eq!(a.resolve("s1", "@e1"), Err(RefError::Stale));
    }
}
