//! In-memory state checkpointing and rollback ($T_{-1}$).
//!
//! Captures page snapshots (URL, title, form input values, localStorage,
//! sessionStorage, cookies, and scroll coordinates) as deep copies, to give
//! browser automation an "undo" for failures without restarting a workflow.
//! This is form state, storage and cookies: it does not capture or restore
//! the DOM tree.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// Captured state of an interactable HTML input/textarea/select element.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FormInputState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub tag: String,
    pub input_type: String,
    pub value: Value,
    pub checked: bool,
    pub selected_index: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub xpath: Option<String>,
}

/// A comprehensive point-in-time snapshot of a browser tab.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub tag: String,
    pub target_id: String,
    pub url: String,
    pub title: String,
    pub timestamp_ms: u64,
    pub cookies: Vec<Value>,
    pub local_storage: Value,
    pub session_storage: Value,
    pub scroll_x: f64,
    pub scroll_y: f64,
    pub inputs: Vec<FormInputState>,
}

/// In-memory ring-buffered store of tab checkpoints.
#[derive(Debug, Default)]
pub struct CheckpointStore {
    checkpoints: HashMap<String, Vec<Checkpoint>>,
}

impl CheckpointStore {
    pub const MAX_CHECKPOINTS_PER_TARGET: usize = 50;

    pub fn new() -> Self {
        Self {
            checkpoints: HashMap::new(),
        }
    }

    pub fn save(&mut self, checkpoint: Checkpoint) {
        let list = self
            .checkpoints
            .entry(checkpoint.target_id.clone())
            .or_default();
        // A re-saved tag is the newest checkpoint: drop the old entry and
        // append, so `latest` / T-1 (the last element) really is the last save.
        list.retain(|c| c.tag != checkpoint.tag);
        list.push(checkpoint);
        if list.len() > Self::MAX_CHECKPOINTS_PER_TARGET {
            list.remove(0);
        }
    }

    pub fn get(&self, target_id: &str, tag: Option<&str>) -> Option<&Checkpoint> {
        let list = self.checkpoints.get(target_id)?;
        if let Some(t) = tag {
            if t == "latest" || t == "T-1" || t == "t-1" {
                list.last()
            } else {
                list.iter().find(|c| c.tag == t)
            }
        } else {
            list.last()
        }
    }

    pub fn list(&self, target_id: Option<&str>) -> Vec<&Checkpoint> {
        match target_id {
            Some(tid) => self
                .checkpoints
                .get(tid)
                .map(|v| v.iter().collect())
                .unwrap_or_default(),
            None => self.checkpoints.values().flat_map(|v| v.iter()).collect(),
        }
    }

    pub fn delete(&mut self, target_id: &str, tag: Option<&str>) -> usize {
        if let Some(t) = tag {
            if t == "*" {
                self.checkpoints
                    .remove(target_id)
                    .map(|v| v.len())
                    .unwrap_or(0)
            } else if let Some(list) = self.checkpoints.get_mut(target_id) {
                let initial = list.len();
                list.retain(|c| c.tag != t);
                initial - list.len()
            } else {
                0
            }
        } else {
            self.checkpoints
                .remove(target_id)
                .map(|v| v.len())
                .unwrap_or(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn checkpoint_save_and_retrieve_t_minus_1() {
        let mut store = CheckpointStore::new();
        let cp1 = Checkpoint {
            tag: "step_1".into(),
            target_id: "tab_1".into(),
            url: "https://example.com/step1".into(),
            title: "Step 1".into(),
            timestamp_ms: 1000,
            cookies: vec![json!({"name": "session", "value": "xyz"})],
            local_storage: json!({"theme": "dark"}),
            session_storage: json!({"cart_id": "c123"}),
            scroll_x: 0.0,
            scroll_y: 120.0,
            inputs: vec![FormInputState {
                id: Some("username".into()),
                name: Some("username".into()),
                tag: "input".into(),
                input_type: "text".into(),
                value: json!("alice"),
                checked: false,
                selected_index: -1,
                xpath: None,
            }],
        };

        let cp2 = Checkpoint {
            tag: "step_2".into(),
            target_id: "tab_1".into(),
            url: "https://example.com/step2".into(),
            title: "Step 2".into(),
            timestamp_ms: 2000,
            cookies: vec![],
            local_storage: json!({}),
            session_storage: json!({}),
            scroll_x: 0.0,
            scroll_y: 200.0,
            inputs: vec![],
        };

        store.save(cp1.clone());
        store.save(cp2.clone());

        // Latest / T-1 returns step_2
        assert_eq!(store.get("tab_1", None).unwrap().tag, "step_2");
        assert_eq!(store.get("tab_1", Some("latest")).unwrap().tag, "step_2");
        assert_eq!(store.get("tab_1", Some("T-1")).unwrap().tag, "step_2");

        // Specific tag returns step_1
        assert_eq!(store.get("tab_1", Some("step_1")).unwrap().tag, "step_1");
        assert_eq!(
            store.get("tab_1", Some("step_1")).unwrap().inputs[0].value,
            json!("alice")
        );

        // Re-saving an existing tag makes it the newest, so `latest` follows it.
        let mut again = cp1.clone();
        again.timestamp_ms = 3000;
        store.save(again);
        assert_eq!(store.get("tab_1", None).unwrap().tag, "step_1");
        assert_eq!(store.get("tab_1", Some("latest")).unwrap().tag, "step_1");
        assert_eq!(store.list(Some("tab_1")).len(), 2);
        store.save(cp2.clone());
        assert_eq!(store.get("tab_1", None).unwrap().tag, "step_2");

        // Delete step_1
        assert_eq!(store.delete("tab_1", Some("step_1")), 1);
        assert!(store.get("tab_1", Some("step_1")).is_none());
        assert_eq!(store.get("tab_1", None).unwrap().tag, "step_2");
    }
}
