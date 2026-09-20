//! Visual-regression baselines: a named reference screenshot (base64 PNG) that
//! a later run diffs against. First run for a name saves the baseline; after
//! that a run compares and reports the changed-pixel ratio. File-backed JSON,
//! keyed by name, so baselines survive across runs and CI checkouts.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One saved baseline screenshot. `w`/`h` are the captured pixel dimensions
/// (0 when the capture did not measure them, e.g. a full-page shot); the diff
/// reads the real dimensions from the decoded image regardless.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Baseline {
    pub name: String,
    pub png_base64: String,
    pub w: u32,
    pub h: u32,
    pub updated_ms: u128,
}

#[derive(Debug)]
pub enum VisualError {
    Invalid(String),
    Io(String),
}

/// File-backed baseline store, keyed by name.
pub struct VisualStore {
    path: PathBuf,
    max_baselines: usize,
}

impl VisualStore {
    pub fn new(path: PathBuf, max_baselines: usize) -> Self {
        VisualStore {
            path,
            max_baselines,
        }
    }

    fn load(&self) -> Result<BTreeMap<String, Baseline>, VisualError> {
        match std::fs::read_to_string(&self.path) {
            Ok(t) => serde_json::from_str(&t)
                .map_err(|e| VisualError::Io(format!("{}: {e}", self.path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(VisualError::Io(format!("{}: {e}", self.path.display()))),
        }
    }

    fn persist(&self, map: &BTreeMap<String, Baseline>) -> Result<(), VisualError> {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let json = serde_json::to_string_pretty(map)
            .map_err(|e| VisualError::Io(format!("serialize: {e}")))?;
        std::fs::write(&self.path, json)
            .map_err(|e| VisualError::Io(format!("{}: {e}", self.path.display())))
    }

    pub fn get(&self, name: &str) -> Result<Option<Baseline>, VisualError> {
        Ok(self.load()?.remove(name.trim()))
    }

    /// Store (or replace) a baseline. A new name at the cap is rejected;
    /// replacing an existing one is always allowed.
    pub fn save(
        &self,
        name: &str,
        png_base64: String,
        w: u32,
        h: u32,
        now_ms: u128,
    ) -> Result<Baseline, VisualError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(VisualError::Invalid(
                "baseline name must not be empty".into(),
            ));
        }
        if png_base64.is_empty() {
            return Err(VisualError::Invalid("baseline image is empty".into()));
        }
        let mut map = self.load()?;
        if !map.contains_key(name) && map.len() >= self.max_baselines {
            return Err(VisualError::Invalid(format!(
                "at most {} baselines may be stored",
                self.max_baselines
            )));
        }
        let b = Baseline {
            name: name.to_string(),
            png_base64,
            w,
            h,
            updated_ms: now_ms,
        };
        map.insert(name.to_string(), b.clone());
        self.persist(&map)?;
        Ok(b)
    }

    pub fn delete(&self, name: &str) -> Result<bool, VisualError> {
        let mut map = self.load()?;
        let removed = map.remove(name.trim()).is_some();
        if removed {
            self.persist(&map)?;
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(tag: &str) -> VisualStore {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "agentctl-baselines-{tag}-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&p);
        VisualStore::new(p, 3)
    }

    #[test]
    fn save_get_replace_delete_round_trip() {
        let s = store("rt");
        assert!(s.get("home").unwrap().is_none());
        let b = s.save("home", "AAAA".into(), 10, 20, 1).unwrap();
        assert_eq!(b.png_base64, "AAAA");
        assert_eq!(s.get("home").unwrap().unwrap().w, 10);
        // Replacing the same name keeps it one entry.
        s.save("home", "BBBB".into(), 30, 40, 2).unwrap();
        assert_eq!(s.get("home").unwrap().unwrap().png_base64, "BBBB");
        assert!(s.delete("home").unwrap());
        assert!(!s.delete("home").unwrap());
    }

    #[test]
    fn empty_name_or_image_is_rejected() {
        let s = store("bad");
        assert!(matches!(
            s.save("", "x".into(), 0, 0, 1),
            Err(VisualError::Invalid(_))
        ));
        assert!(matches!(
            s.save("n", "".into(), 0, 0, 1),
            Err(VisualError::Invalid(_))
        ));
    }

    #[test]
    fn the_baseline_cap_is_enforced_but_replacement_is_not() {
        let s = store("cap");
        s.save("a", "x".into(), 0, 0, 1).unwrap();
        s.save("b", "x".into(), 0, 0, 1).unwrap();
        s.save("c", "x".into(), 0, 0, 1).unwrap();
        assert!(s.save("d", "x".into(), 0, 0, 1).is_err(), "cap is 3");
        assert!(
            s.save("a", "y".into(), 0, 0, 2).is_ok(),
            "replace at cap is ok"
        );
    }
}
