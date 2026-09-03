use std::collections::{HashMap, VecDeque};

use serde::Serialize;

use crate::tree::Bounds;

/// How many superseded snapshots are retained for diffing.
///
/// Enough that an agent can compare against something it observed a few turns
/// ago; small enough that a long session does not accumulate UI trees. These
/// are kept for *comparison only* — see [`SnapshotArena::get`].
const HISTORY: usize = 8;

/// The parts of an element's state a diff should notice.
///
/// Separated from the identifying fields so a change in any of them reads as
/// "this element changed", not "one element vanished and another appeared".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ElementState {
    pub focused: bool,
    pub disabled: bool,
    pub selected: bool,
    pub checked: Option<bool>,
    pub expanded: Option<bool>,
}

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
    /// Toggles and flags, compared when diffing snapshots.
    #[serde(default)]
    pub state: ElementState,
}

/// One captured snapshot: an id, the app/window it came from, and the `@eN` ->
/// info map.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub id: String,
    pub app: Option<String>,
    pub window: Option<String>,
    pub elements: HashMap<String, ElementInfo>,
    /// Whether this snapshot was taken in depth-limited overview mode. A
    /// skeleton and a full tree are not comparable, so a diff across the two
    /// says so instead of reporting the missing detail as deletions.
    pub skeleton: bool,
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
    /// Superseded snapshots, newest first, for diffing only.
    history: VecDeque<Snapshot>,
    counter: u64,
}

impl SnapshotArena {
    pub fn new() -> Self {
        SnapshotArena::default()
    }

    /// The next snapshot id. Lives here so every observation, from whichever
    /// tool, draws from one sequence.
    pub fn next_id(&mut self) -> String {
        self.counter += 1;
        format!("s{:x}", self.counter)
    }

    /// Install a new snapshot, superseding any previous one.
    ///
    /// The superseded snapshot moves to the history, where it can be *compared*
    /// against but never resolved: its refs and backend handles belong to a UI
    /// that has moved on.
    pub fn install(&mut self, snapshot: Snapshot) {
        if let Some(old) = self.current.take() {
            self.history.push_front(old);
            while self.history.len() >= HISTORY {
                self.history.pop_back();
            }
        }
        self.current = Some(snapshot);
    }

    /// Look up a snapshot by id, current or retained, **for diffing only**.
    ///
    /// Deliberately not a resolution path. A retained snapshot's `@eN` refs
    /// point into a tree that no longer exists, and its backend node ids were
    /// invalidated the moment the next observation replaced them — acting on
    /// one would target nothing, or something else.
    pub fn get(&self, id: &str) -> Option<&Snapshot> {
        self.current
            .as_ref()
            .filter(|s| s.id == id)
            .or_else(|| self.history.iter().find(|s| s.id == id))
    }

    /// Ids currently available to `get`, newest first.
    pub fn known_ids(&self) -> Vec<String> {
        self.current
            .iter()
            .chain(self.history.iter())
            .map(|s| s.id.clone())
            .collect()
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
            state: ElementState::default(),
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
            skeleton: false,
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
