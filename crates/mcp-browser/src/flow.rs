//! Saved browser UI tests: a named sequence of steps (navigate / act / wait /
//! capture / assert) recorded once and replayed deterministically. Replay only
//! calls the deterministic engine, so a green run never needs a model; a
//! failing step stops the run and names itself, which is where an agent takes
//! over. File-backed JSON, keyed by name.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One saved flow. `steps` are opaque step objects (see the `browser_flow`
/// schema); keeping them as raw JSON means the store never has to change when
/// a step gains a field.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Flow {
    pub name: String,
    pub steps: Vec<Value>,
    pub updated_ms: u128,
}

#[derive(Debug)]
pub enum FlowError {
    Invalid(String),
    Io(String),
}

/// File-backed flow store, keyed by name.
pub struct FlowStore {
    path: PathBuf,
    max_flows: usize,
    max_steps: usize,
}

impl FlowStore {
    pub fn new(path: PathBuf, max_flows: usize, max_steps: usize) -> Self {
        FlowStore {
            path,
            max_flows,
            max_steps,
        }
    }

    fn load(&self) -> Result<BTreeMap<String, Flow>, FlowError> {
        match std::fs::read_to_string(&self.path) {
            Ok(t) => serde_json::from_str(&t)
                .map_err(|e| FlowError::Io(format!("{}: {e}", self.path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(FlowError::Io(format!("{}: {e}", self.path.display()))),
        }
    }

    fn persist(&self, map: &BTreeMap<String, Flow>) -> Result<(), FlowError> {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let json = serde_json::to_string_pretty(map)
            .map_err(|e| FlowError::Io(format!("serialize: {e}")))?;
        std::fs::write(&self.path, json)
            .map_err(|e| FlowError::Io(format!("{}: {e}", self.path.display())))
    }

    pub fn save(&self, name: &str, steps: Vec<Value>, now_ms: u128) -> Result<Flow, FlowError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(FlowError::Invalid("flow name must not be empty".into()));
        }
        if steps.is_empty() {
            return Err(FlowError::Invalid("a flow needs at least one step".into()));
        }
        if steps.len() > self.max_steps {
            return Err(FlowError::Invalid(format!(
                "a flow may have at most {} steps",
                self.max_steps
            )));
        }
        let mut map = self.load()?;
        if !map.contains_key(name) && map.len() >= self.max_flows {
            return Err(FlowError::Invalid(format!(
                "at most {} flows may be stored",
                self.max_flows
            )));
        }
        let flow = Flow {
            name: name.to_string(),
            steps,
            updated_ms: now_ms,
        };
        map.insert(name.to_string(), flow.clone());
        self.persist(&map)?;
        Ok(flow)
    }

    pub fn get(&self, name: &str) -> Result<Option<Flow>, FlowError> {
        Ok(self.load()?.remove(name.trim()))
    }

    pub fn list(&self) -> Result<Vec<Flow>, FlowError> {
        let mut v: Vec<Flow> = self.load()?.into_values().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(v)
    }

    pub fn delete(&self, name: &str) -> Result<bool, FlowError> {
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
    use serde_json::json;

    fn store(tag: &str) -> FlowStore {
        let mut p = std::env::temp_dir();
        p.push(format!("agentctl-flows-{tag}-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&p);
        FlowStore::new(p, 10, 50)
    }

    #[test]
    fn save_get_list_delete_round_trip() {
        let s = store("rt");
        let steps = vec![
            json!({"op":"navigate","url":"https://x"}),
            json!({"op":"assert","text":"hi"}),
        ];
        let f = s.save("login", steps.clone(), 1).unwrap();
        assert_eq!(f.steps.len(), 2);
        assert_eq!(s.get("login").unwrap().unwrap().steps, steps);
        assert_eq!(s.list().unwrap().len(), 1);
        assert!(s.delete("login").unwrap());
        assert!(s.get("login").unwrap().is_none());
        assert!(!s.delete("login").unwrap(), "second delete is a no-op");
    }

    #[test]
    fn saving_again_replaces_the_steps() {
        let s = store("replace");
        s.save("f", vec![json!({"op":"navigate"})], 1).unwrap();
        s.save("f", vec![json!({"op":"assert"}), json!({"op":"wait"})], 2)
            .unwrap();
        assert_eq!(s.get("f").unwrap().unwrap().steps.len(), 2);
        assert_eq!(s.list().unwrap().len(), 1, "same name is one flow");
    }

    #[test]
    fn empty_name_or_no_steps_is_rejected() {
        let s = store("bad");
        assert!(matches!(
            s.save("", vec![json!({})], 1),
            Err(FlowError::Invalid(_))
        ));
        assert!(matches!(s.save("x", vec![], 1), Err(FlowError::Invalid(_))));
    }

    #[test]
    fn the_step_and_flow_caps_are_enforced() {
        let s = FlowStore::new(
            std::env::temp_dir().join(format!("agentctl-flows-cap-{}.json", std::process::id())),
            2,
            3,
        );
        let _ = std::fs::remove_file(
            std::env::temp_dir().join(format!("agentctl-flows-cap-{}.json", std::process::id())),
        );
        assert!(s.save("too-many-steps", vec![json!({}); 4], 1).is_err());
        s.save("a", vec![json!({})], 1).unwrap();
        s.save("b", vec![json!({})], 1).unwrap();
        assert!(s.save("c", vec![json!({})], 1).is_err(), "flow cap is 2");
        // Overwriting an existing flow is allowed at the cap.
        assert!(s.save("a", vec![json!({})], 2).is_ok());
    }
}
