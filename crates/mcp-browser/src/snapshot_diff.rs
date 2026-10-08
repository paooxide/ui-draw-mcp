//! Snapshot diffs. A model that acts on a page re-reads it after every step,
//! and almost all of a 400-node snapshot is the same as the one before. The
//! module remembers the last snapshot it returned for each tab, and
//! `browser_snapshot diff` answers with what moved since: nodes added,
//! removed, and the fields that changed on the ones that stayed.

use std::collections::{HashMap, VecDeque};

use serde_json::{json, Map, Value};

/// Tabs whose last snapshot is kept. A snapshot is at most 400 nodes, so this
/// bounds the memory; the oldest tab is dropped first.
const MAX_REMEMBERED: usize = 16;

/// Node keys that are not compared: where a node sits shifts whenever the
/// layout does, and says nothing about the node having changed.
const IGNORED: [&str; 4] = ["x", "y", "w", "h"];

struct Remembered {
    url: String,
    title: String,
    /// What the snapshot covered (mode and root); a diff only makes sense
    /// between two of the same.
    scope: String,
    nodes: Vec<Value>,
}

/// The last snapshot returned for each tab, oldest first out.
#[derive(Default)]
pub(crate) struct SnapMemory {
    order: VecDeque<String>,
    map: HashMap<String, Remembered>,
}

impl SnapMemory {
    fn remember(&mut self, target: &str, r: Remembered) {
        if self.map.insert(target.to_string(), r).is_none() {
            self.order.push_back(target.to_string());
        }
        while self.order.len() > MAX_REMEMBERED {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
    }

    /// A tab was closed.
    pub(crate) fn forget(&mut self, target: &str) {
        self.map.remove(target);
        self.order.retain(|t| t != target);
    }

    /// Every tab of a browser is gone with its connection.
    pub(crate) fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }
}

/// The URL without its fragment: moving to `#section` is not a new document.
fn document_url(url: &str) -> &str {
    url.split('#').next().unwrap_or(url)
}

/// The reply to a `browser_snapshot`: `snap` itself, or with `want_diff` what
/// changed since the last one returned for `target` when that is possible
/// (and smaller). The snapshot is remembered either way. A full reply to a
/// diff request says `diff: "full"` and why.
pub(crate) fn reply(
    mem: &mut SnapMemory,
    target: &str,
    scope: String,
    mut snap: Value,
    want_diff: bool,
) -> Value {
    let Some(nodes) = snap.get("nodes").and_then(Value::as_array).cloned() else {
        return snap;
    };
    let url = snap
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let title = snap
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let prev = mem.map.get(target);
    let why_full = if !want_diff {
        None
    } else if let Some(p) = prev {
        if p.scope != scope {
            Some("mode or root_selector differs from the last snapshot")
        } else if document_url(&p.url) != document_url(&url) {
            Some("navigated to a different document")
        } else {
            None
        }
    } else {
        Some("no previous snapshot of this tab")
    };
    let mut out = None;
    if let (true, None, Some(p)) = (want_diff, why_full, prev) {
        let d = delta(&p.nodes, &nodes);
        let mut o = Map::new();
        o.insert("diff".into(), json!("delta"));
        if url != p.url {
            o.insert("url".into(), json!(url));
        }
        if title != p.title {
            o.insert("title".into(), json!(title));
        }
        o.insert("unchanged".into(), json!(d.unchanged));
        for (k, v) in [
            ("added", d.added),
            ("removed", d.removed),
            ("changed", d.changed),
        ] {
            if !v.is_empty() {
                o.insert(k.into(), Value::Array(v));
            }
        }
        let delta = Value::Object(o);
        if json_len(&delta) < json_len(&snap) {
            out = Some(delta);
        } else {
            snap["diff"] = json!("full");
            snap["reason"] = json!("the diff was not smaller than the snapshot");
            out = Some(snap.clone());
        }
    } else if let Some(reason) = why_full {
        snap["diff"] = json!("full");
        snap["reason"] = json!(reason);
    }
    mem.remember(
        target,
        Remembered {
            url,
            title,
            scope,
            nodes,
        },
    );
    out.unwrap_or(snap)
}

fn json_len(v: &Value) -> usize {
    serde_json::to_string(v).map_or(usize::MAX, |s| s.len())
}

struct Delta {
    added: Vec<Value>,
    removed: Vec<Value>,
    changed: Vec<Value>,
    unchanged: usize,
}

fn field<'a>(n: &'a Value, k: &str) -> &'a str {
    n.get(k).and_then(Value::as_str).unwrap_or_default()
}

/// Pair up the nodes of two snapshots, then say what differs. Refs are
/// positional XPaths, so one inserted sibling shifts every ref after it; a
/// node is therefore matched by ref only when its tag and name agree, then by
/// tag, role and name among those left (a moved node), then by ref and tag
/// alone (a node whose name changed).
fn delta(prev: &[Value], cur: &[Value]) -> Delta {
    let mut pair: Vec<Option<usize>> = vec![None; cur.len()];
    let mut taken = vec![false; prev.len()];
    let by_ref: HashMap<&str, usize> = prev
        .iter()
        .enumerate()
        .map(|(i, n)| (field(n, "ref"), i))
        .collect();
    for (j, c) in cur.iter().enumerate() {
        if let Some(&i) = by_ref.get(field(c, "ref")) {
            let p = &prev[i];
            if !taken[i]
                && field(p, "tag") == field(c, "tag")
                && field(p, "name") == field(c, "name")
            {
                pair[j] = Some(i);
                taken[i] = true;
            }
        }
    }
    let mut by_key: HashMap<(&str, &str, &str), VecDeque<usize>> = HashMap::new();
    for (i, p) in prev.iter().enumerate() {
        if !taken[i] && !field(p, "name").is_empty() {
            by_key
                .entry((field(p, "tag"), field(p, "role"), field(p, "name")))
                .or_default()
                .push_back(i);
        }
    }
    for (j, c) in cur.iter().enumerate() {
        if pair[j].is_some() || field(c, "name").is_empty() {
            continue;
        }
        let key = (field(c, "tag"), field(c, "role"), field(c, "name"));
        if let Some(i) = by_key.get_mut(&key).and_then(VecDeque::pop_front) {
            pair[j] = Some(i);
            taken[i] = true;
        }
    }
    for (j, c) in cur.iter().enumerate() {
        if pair[j].is_some() {
            continue;
        }
        if let Some(&i) = by_ref.get(field(c, "ref")) {
            if !taken[i] && field(&prev[i], "tag") == field(c, "tag") {
                pair[j] = Some(i);
                taken[i] = true;
            }
        }
    }

    let mut d = Delta {
        added: vec![],
        removed: vec![],
        changed: vec![],
        unchanged: 0,
    };
    for (j, c) in cur.iter().enumerate() {
        let Some(i) = pair[j] else {
            d.added.push(c.clone());
            continue;
        };
        let changes = field_changes(&prev[i], c);
        if changes.is_empty() {
            d.unchanged += 1;
            continue;
        }
        let mut e = Map::new();
        e.insert("ref".into(), json!(field(c, "ref")));
        if !field(c, "name").is_empty() {
            e.insert("name".into(), json!(field(c, "name")));
        }
        e.insert("changes".into(), Value::Object(changes));
        d.changed.push(Value::Object(e));
    }
    for (i, p) in prev.iter().enumerate() {
        if !taken[i] {
            let mut e = Map::new();
            e.insert("ref".into(), json!(field(p, "ref")));
            e.insert("tag".into(), json!(field(p, "tag")));
            if !field(p, "name").is_empty() {
                e.insert("name".into(), json!(field(p, "name")));
            }
            d.removed.push(Value::Object(e));
        }
    }
    d
}

/// `{field: [was, now]}` for every compared key that differs; a key a node
/// does not carry reads as null (the snapshot leaves out empty fields).
fn field_changes(a: &Value, b: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    let (Some(a), Some(b)) = (a.as_object(), b.as_object()) else {
        return out;
    };
    let mut keys: Vec<&String> = a.keys().chain(b.keys()).collect();
    keys.sort();
    keys.dedup();
    for k in keys {
        if IGNORED.contains(&k.as_str()) {
            continue;
        }
        let (was, now) = (
            a.get(k).unwrap_or(&Value::Null),
            b.get(k).unwrap_or(&Value::Null),
        );
        if was != now {
            out.insert(k.clone(), json!([was, now]));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(r: &str, tag: &str, name: &str) -> Value {
        json!({ "ref": r, "tag": tag, "name": name, "x": 1, "y": 2, "w": 3, "h": 4 })
    }

    fn snap(url: &str, nodes: Vec<Value>) -> Value {
        json!({ "url": url, "title": "t", "nodes": nodes })
    }

    /// Enough unchanged nodes that a one-node diff is clearly smaller.
    fn base(extra: Vec<Value>) -> Vec<Value> {
        let mut v: Vec<Value> = (1..=12)
            .map(|i| {
                node(
                    &format!("/html/body/p[{i}]"),
                    "p",
                    &format!("paragraph {i}"),
                )
            })
            .collect();
        v.extend(extra);
        v
    }

    fn mem_with(nodes: Vec<Value>) -> SnapMemory {
        let mut m = SnapMemory::default();
        reply(&mut m, "T", "dom:".into(), snap("http://a/", nodes), false);
        m
    }

    #[test]
    fn a_changed_field_is_the_only_thing_reported() {
        let mut m = mem_with(base(vec![
            json!({ "ref": "//*[@id='q']", "tag": "input", "name": "Search" }),
        ]));
        let now = base(vec![
            json!({ "ref": "//*[@id='q']", "tag": "input", "name": "Search", "value": "hi" }),
        ]);
        let d = reply(&mut m, "T", "dom:".into(), snap("http://a/", now), true);
        assert_eq!(d["diff"], "delta");
        assert_eq!(d["unchanged"], 12);
        assert_eq!(d["changed"][0]["changes"]["value"], json!([null, "hi"]));
        assert!(d.get("added").is_none() && d.get("removed").is_none());
        assert!(d.get("url").is_none(), "{d}");
    }

    #[test]
    fn moving_is_not_a_change_but_a_shifted_ref_is_reported() {
        let mut m = mem_with(base(vec![node("/html/body/button[1]", "button", "Save")]));
        let mut now = base(vec![node("/html/body/button[2]", "button", "Save")]);
        now[0]["x"] = json!(99);
        let d = reply(&mut m, "T", "dom:".into(), snap("http://a/", now), true);
        assert_eq!(d["unchanged"], 12, "{d}");
        assert_eq!(
            d["changed"][0]["changes"]["ref"],
            json!(["/html/body/button[1]", "/html/body/button[2]"])
        );
        assert!(
            d.get("added").is_none() && d.get("removed").is_none(),
            "{d}"
        );
    }

    #[test]
    fn nodes_that_appear_and_vanish_are_added_and_removed() {
        let mut m = mem_with(base(vec![node("/html/body/a", "a", "Old")]));
        let now = base(vec![node("/html/body/div/b", "button", "New")]);
        let d = reply(&mut m, "T", "dom:".into(), snap("http://a/", now), true);
        assert_eq!(d["added"][0]["name"], "New");
        assert_eq!(d["removed"][0]["name"], "Old");
        assert_eq!(d["removed"][0]["ref"], "/html/body/a");
    }

    #[test]
    fn first_snapshot_other_page_and_other_scope_come_back_full() {
        let mut m = SnapMemory::default();
        let s = snap("http://a/", base(vec![]));
        let r = reply(&mut m, "T", "dom:".into(), s.clone(), true);
        assert_eq!(r["diff"], "full");
        assert!(r["reason"].as_str().unwrap().contains("no previous"));
        assert_eq!(r["nodes"].as_array().unwrap().len(), 12);

        let other = reply(
            &mut m,
            "T",
            "dom:".into(),
            snap("http://b/", base(vec![])),
            true,
        );
        assert_eq!(other["diff"], "full");
        assert!(other["reason"]
            .as_str()
            .unwrap()
            .contains("different document"));

        let scoped = reply(
            &mut m,
            "T",
            "dom:#x".into(),
            snap("http://b/", base(vec![])),
            true,
        );
        assert!(scoped["reason"].as_str().unwrap().contains("root_selector"));

        // A fragment is the same document.
        let frag = reply(
            &mut m,
            "T",
            "dom:#x".into(),
            snap("http://b/#top", base(vec![])),
            true,
        );
        assert_eq!(frag["diff"], "delta", "{frag}");
    }

    #[test]
    fn plain_snapshots_are_untouched_and_remembered() {
        let mut m = SnapMemory::default();
        let s = snap("http://a/", base(vec![]));
        assert_eq!(reply(&mut m, "T", "dom:".into(), s.clone(), false), s);
        let d = reply(&mut m, "T", "dom:".into(), s, true);
        assert_eq!(d["diff"], "delta");
        assert_eq!(d["unchanged"], 12);
    }

    #[test]
    fn a_diff_no_smaller_than_the_snapshot_is_sent_as_the_snapshot() {
        let mut m = mem_with(vec![node("/a", "a", "One"), node("/b", "a", "Two")]);
        let now = vec![node("/c", "button", "Three"), node("/d", "button", "Four")];
        let r = reply(&mut m, "T", "dom:".into(), snap("http://a/", now), true);
        assert_eq!(r["diff"], "full", "{r}");
        assert_eq!(r["nodes"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn memory_is_bounded_and_forgets_closed_tabs() {
        let mut m = SnapMemory::default();
        for i in 0..(MAX_REMEMBERED + 5) {
            reply(
                &mut m,
                &format!("T{i}"),
                "dom:".into(),
                snap("http://a/", vec![]),
                false,
            );
        }
        assert_eq!(m.map.len(), MAX_REMEMBERED);
        assert!(!m.map.contains_key("T0"));
        m.forget("T20");
        assert!(!m.map.contains_key("T20"));
        assert_eq!(m.order.len(), m.map.len());
        m.clear();
        assert!(m.map.is_empty() && m.order.is_empty());
    }
}
