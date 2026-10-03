//! Comparing two snapshots, so an agent can ask what changed rather than
//! re-reading the whole UI.
//!
//! After an action, almost nothing on screen is different. Re-observing costs
//! the same as the first observation anyway, which is why the cheap thing to
//! send back is the difference.
//!
//! **What the diff keys on, and why it is not the ref.** `@eN` refs are
//! allocated in document order, so inserting one row renumbers everything below
//! it and a ref-keyed diff would report the entire lower half of the window as
//! removed and re-added. Elements are matched on `role` + `name`, with an
//! ordinal to separate the several buttons that are all called "Delete".
//!
//! **What is compared rather than keyed on.** Bounds, value, secure and the
//! state flags. Bounds especially must not be part of the key: moving a window
//! changes every element's position, and that is one event, not "everything was
//! replaced".

use serde::Serialize;
use serde_json::{json, Value};

use crate::arena::{ElementInfo, Snapshot};
use crate::tree::Bounds;

/// How many entries of each kind are reported before truncating. A diff that
/// is bigger than the tree has failed at its job.
const LIST_CAP: usize = 100;

/// One added, removed or changed element.
#[derive(Debug, Clone, Serialize)]
pub struct DeltaEntry {
    /// The ref in the *newer* snapshot. Absent for a removed element, whose ref
    /// belongs to a tree that no longer exists and would resolve to nothing.
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    pub reff: Option<String>,
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bounds: Option<Bounds>,
    /// Which fields differ. Only present for a change.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<&'static str>,
    /// The previous values of exactly those fields, so the agent can see what
    /// it changed *from* without having kept the old snapshot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<Value>,
}

/// The result of comparing two snapshots.
#[derive(Debug, Clone, Serialize)]
pub struct SnapshotDelta {
    pub before_id: String,
    pub after_id: String,
    pub added: Vec<DeltaEntry>,
    pub removed: Vec<DeltaEntry>,
    pub changed: Vec<DeltaEntry>,
    /// Elements present in both and identical.
    pub unchanged: usize,
    /// True when the two snapshots are not really comparable — a different app
    /// or window, or one taken in skeleton mode. The diff is still returned,
    /// but its size means "you are looking at something else", not "the UI
    /// changed enormously".
    pub scope_changed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Identity of an element for matching purposes: role, name, and which of the
/// identically-named ones this is.
fn identity(info: &ElementInfo, ordinal: usize) -> (String, String, usize) {
    (
        info.role.clone(),
        info.name.clone().unwrap_or_default(),
        ordinal,
    )
}

/// Numeric part of `@eN`, so elements are walked in document order — which is
/// what makes the ordinal stable between two snapshots of the same UI.
fn ref_order(reff: &str) -> u64 {
    reff.trim_start_matches("@e").parse().unwrap_or(u64::MAX)
}

/// Elements keyed by identity, in document order.
fn keyed(snap: &Snapshot) -> Vec<((String, String, usize), &String, &ElementInfo)> {
    let mut refs: Vec<(&String, &ElementInfo)> = snap.elements.iter().collect();
    refs.sort_by_key(|(r, _)| ref_order(r));
    let mut seen: std::collections::HashMap<(String, String), usize> =
        std::collections::HashMap::new();
    refs.into_iter()
        .map(|(r, info)| {
            let base = (info.role.clone(), info.name.clone().unwrap_or_default());
            let n = seen.entry(base).or_insert(0);
            let key = identity(info, *n);
            *n += 1;
            (key, r, info)
        })
        .collect()
}

fn entry(reff: Option<String>, info: &ElementInfo) -> DeltaEntry {
    DeltaEntry {
        reff,
        role: info.role.clone(),
        name: info.name.clone(),
        value: info.value_preview.clone(),
        bounds: info.bounds,
        fields: Vec::new(),
        before: None,
    }
}

/// Which comparable fields differ between two versions of the same element.
fn differences(a: &ElementInfo, b: &ElementInfo) -> (Vec<&'static str>, Value) {
    let mut fields = Vec::new();
    let mut before = serde_json::Map::new();
    if a.value_preview != b.value_preview {
        fields.push("value");
        before.insert("value".into(), json!(a.value_preview));
    }
    if a.bounds != b.bounds {
        fields.push("bounds");
        before.insert("bounds".into(), json!(a.bounds));
    }
    if a.secure != b.secure {
        fields.push("secure");
        before.insert("secure".into(), json!(a.secure));
    }
    if a.state != b.state {
        fields.push("state");
        before.insert("state".into(), json!(a.state));
    }
    (fields, Value::Object(before))
}

/// Compare two snapshots.
pub fn diff_snapshots(before: &Snapshot, after: &Snapshot) -> SnapshotDelta {
    let scope_changed = before.app != after.app
        || before.window != after.window
        || before.skeleton != after.skeleton;
    let note = if scope_changed {
        Some(if before.skeleton != after.skeleton {
            "one snapshot is a depth-limited skeleton; the two are not directly comparable".into()
        } else {
            format!(
                "scope changed: was {:?}/{:?}, now {:?}/{:?}",
                before.app, before.window, after.app, after.window
            )
        })
    } else {
        None
    };

    let old = keyed(before);
    let new = keyed(after);
    let old_map: std::collections::HashMap<_, _> =
        old.iter().map(|(k, r, i)| (k.clone(), (*r, *i))).collect();
    let new_map: std::collections::HashMap<_, _> =
        new.iter().map(|(k, r, i)| (k.clone(), (*r, *i))).collect();

    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    let mut unchanged = 0usize;

    for (key, reff, info) in &new {
        match old_map.get(key) {
            None => added.push(entry(Some((*reff).clone()), info)),
            Some((_, old_info)) => {
                let (fields, before_vals) = differences(old_info, info);
                if fields.is_empty() {
                    unchanged += 1;
                } else {
                    let mut e = entry(Some((*reff).clone()), info);
                    e.fields = fields;
                    e.before = Some(before_vals);
                    changed.push(e);
                }
            }
        }
    }
    for (key, _, info) in &old {
        if !new_map.contains_key(key) {
            // No ref: it belongs to the older tree and cannot be acted on.
            removed.push(entry(None, info));
        }
    }

    SnapshotDelta {
        before_id: before.id.clone(),
        after_id: after.id.clone(),
        added,
        removed,
        changed,
        unchanged,
        scope_changed,
        note,
    }
}

impl SnapshotDelta {
    /// True when nothing observable differs.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }

    /// JSON with each list capped, and the counts that were dropped reported —
    /// a truncated list that says it is complete is worse than no list.
    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "before_id": self.before_id,
            "after_id": self.after_id,
            "added": self.added.iter().take(LIST_CAP).collect::<Vec<_>>(),
            "removed": self.removed.iter().take(LIST_CAP).collect::<Vec<_>>(),
            "changed": self.changed.iter().take(LIST_CAP).collect::<Vec<_>>(),
            "counts": {
                "added": self.added.len(),
                "removed": self.removed.len(),
                "changed": self.changed.len(),
                "unchanged": self.unchanged,
            },
            "empty": self.is_empty(),
        });
        if self.added.len() > LIST_CAP
            || self.removed.len() > LIST_CAP
            || self.changed.len() > LIST_CAP
        {
            v["truncated"] = json!(true);
        }
        if self.scope_changed {
            v["scope_changed"] = json!(true);
        }
        if let Some(n) = &self.note {
            v["note"] = json!(n);
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::ElementState;
    use std::collections::HashMap;

    fn el(role: &str, name: &str, value: Option<&str>) -> ElementInfo {
        ElementInfo {
            role: role.into(),
            name: Some(name.into()),
            value_preview: value.map(str::to_string),
            secure: false,
            bounds: None,
            node_id: Some(1),
            state: ElementState::default(),
            semantic_intent: None,
            bound_state: None,
        }
    }

    fn snap(id: &str, items: &[(&str, ElementInfo)]) -> Snapshot {
        Snapshot {
            id: id.into(),
            app: Some("App".into()),
            window: Some("Win".into()),
            skeleton: false,
            elements: items
                .iter()
                .map(|(r, i)| ((*r).to_string(), i.clone()))
                .collect::<HashMap<_, _>>(),
        }
    }

    #[test]
    fn an_unchanged_ui_produces_an_empty_delta() {
        let a = snap("s1", &[("@e1", el("button", "Save", None))]);
        let b = snap("s2", &[("@e1", el("button", "Save", None))]);
        let d = diff_snapshots(&a, &b);
        assert!(d.is_empty());
        assert_eq!(d.unchanged, 1);
    }

    #[test]
    fn a_typed_value_reads_as_a_change_with_its_previous_value() {
        let a = snap("s1", &[("@e1", el("textarea", "Body", Some("")))]);
        let b = snap("s2", &[("@e1", el("textarea", "Body", Some("hello")))]);
        let d = diff_snapshots(&a, &b);
        assert_eq!(d.changed.len(), 1);
        assert_eq!(d.changed[0].fields, vec!["value"]);
        assert_eq!(d.changed[0].before.as_ref().unwrap()["value"], json!(""));
        assert!(d.added.is_empty() && d.removed.is_empty());
    }

    #[test]
    fn appearing_and_disappearing_elements_are_reported_as_such() {
        let a = snap("s1", &[("@e1", el("button", "Save", None))]);
        let b = snap(
            "s2",
            &[
                ("@e1", el("button", "Save", None)),
                ("@e2", el("button", "Cancel", None)),
            ],
        );
        let d = diff_snapshots(&a, &b);
        assert_eq!(d.added.len(), 1);
        assert_eq!(d.added[0].name.as_deref(), Some("Cancel"));
        assert_eq!(d.added[0].reff.as_deref(), Some("@e2"));

        let back = diff_snapshots(&b, &a);
        assert_eq!(back.removed.len(), 1);
        assert!(
            back.removed[0].reff.is_none(),
            "a removed element has no usable ref"
        );
    }

    /// The reason the diff is not keyed on refs: inserting one element
    /// renumbers everything after it, and a ref-keyed diff would call that a
    /// wholesale replacement.
    #[test]
    fn renumbering_after_an_insertion_is_not_a_wholesale_change() {
        let a = snap(
            "s1",
            &[
                ("@e1", el("button", "One", None)),
                ("@e2", el("button", "Two", None)),
            ],
        );
        // A new element takes @e1; the others shift down.
        let b = snap(
            "s2",
            &[
                ("@e1", el("button", "Zero", None)),
                ("@e2", el("button", "One", None)),
                ("@e3", el("button", "Two", None)),
            ],
        );
        let d = diff_snapshots(&a, &b);
        assert_eq!(d.added.len(), 1, "only the new button is new");
        assert_eq!(d.added[0].name.as_deref(), Some("Zero"));
        assert!(d.removed.is_empty());
        assert_eq!(d.unchanged, 2);
    }

    /// Moving a window changes every element's bounds. That is one event, so
    /// it must read as changes rather than as everything being replaced.
    #[test]
    fn moving_a_window_reports_bounds_changes_not_replacements() {
        let mut a1 = el("button", "Save", None);
        a1.bounds = Some(Bounds {
            x: 0.0,
            y: 0.0,
            w: 10.0,
            h: 10.0,
        });
        let mut b1 = el("button", "Save", None);
        b1.bounds = Some(Bounds {
            x: 300.0,
            y: 200.0,
            w: 10.0,
            h: 10.0,
        });
        let d = diff_snapshots(&snap("s1", &[("@e1", a1)]), &snap("s2", &[("@e1", b1)]));
        assert!(d.added.is_empty() && d.removed.is_empty());
        assert_eq!(d.changed[0].fields, vec!["bounds"]);
    }

    /// Several controls share a name — every row has a "Delete". The ordinal
    /// keeps them apart so a change to the second is not read as a change to
    /// the first.
    #[test]
    fn identically_named_elements_are_told_apart_by_position() {
        let a = snap(
            "s1",
            &[
                ("@e1", el("button", "Delete", Some("row1"))),
                ("@e2", el("button", "Delete", Some("row2"))),
            ],
        );
        let b = snap(
            "s2",
            &[
                ("@e1", el("button", "Delete", Some("row1"))),
                ("@e2", el("button", "Delete", Some("CHANGED"))),
            ],
        );
        let d = diff_snapshots(&a, &b);
        assert_eq!(d.changed.len(), 1);
        assert_eq!(d.changed[0].reff.as_deref(), Some("@e2"));
        assert_eq!(d.unchanged, 1);
    }

    #[test]
    fn a_state_flag_change_is_noticed() {
        let a = snap("s1", &[("@e1", el("checkbox", "Wrap", None))]);
        let mut on = el("checkbox", "Wrap", None);
        on.state.checked = Some(true);
        let b = snap("s2", &[("@e1", on)]);
        let d = diff_snapshots(&a, &b);
        assert_eq!(d.changed[0].fields, vec!["state"]);
    }

    /// Comparing across apps, windows or detail levels is a category error.
    /// The diff still comes back, but flagged, so its size is not mistaken for
    /// a huge UI change.
    #[test]
    fn comparing_different_scopes_is_flagged() {
        let a = snap("s1", &[("@e1", el("button", "Save", None))]);
        let mut b = snap("s2", &[("@e1", el("button", "Save", None))]);
        b.window = Some("Other".into());
        let d = diff_snapshots(&a, &b);
        assert!(d.scope_changed);
        assert!(d.note.is_some());

        let mut c = snap("s3", &[("@e1", el("button", "Save", None))]);
        c.skeleton = true;
        let d = diff_snapshots(&a, &c);
        assert!(d.note.unwrap().contains("skeleton"));
    }

    #[test]
    fn oversized_lists_are_capped_and_say_so() {
        let many: Vec<(String, ElementInfo)> = (1..=LIST_CAP + 20)
            .map(|i| (format!("@e{i}"), el("button", &format!("b{i}"), None)))
            .collect();
        let refs: Vec<(&str, ElementInfo)> =
            many.iter().map(|(r, i)| (r.as_str(), i.clone())).collect();
        let a = snap("s1", &[]);
        let b = snap("s2", &refs);
        let v = diff_snapshots(&a, &b).to_json();
        assert_eq!(v["added"].as_array().unwrap().len(), LIST_CAP);
        assert_eq!(v["counts"]["added"], json!(LIST_CAP + 20));
        assert_eq!(v["truncated"], json!(true));
    }
}
