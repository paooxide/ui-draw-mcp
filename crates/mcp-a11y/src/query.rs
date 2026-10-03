//! Querying a snapshot instead of reading all of it.
//!
//! `get_ui_tree` answers "what is on screen", and for a busy application that
//! is thousands of characters the agent pays for on every turn. Most of the
//! time the actual question is narrower: *where is the Save button*. This
//! module answers that one.
//!
//! The matching is deliberately forgiving in the directions a caller is likely
//! to be imprecise — case, substring, role synonyms — and exact in the one that
//! matters, which is that a returned ref must be usable by `ui_action`.

use serde::Serialize;
use serde_json::{json, Value};

use crate::arena::{ElementInfo, Snapshot};
use crate::tree::{normalize_role, Bounds};

/// Default and maximum number of matches returned.
const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 200;

/// A parsed `find_elements` query.
#[derive(Debug, Clone, Default)]
pub struct ElementQuery {
    /// Role to match, already normalised (`button`, `textfield`, …).
    pub role: Option<String>,
    /// Case-insensitive substring of the element's name.
    pub name: Option<String>,
    /// Rank by distance from this screen point rather than by tree order.
    pub near: Option<(f64, f64)>,
    /// Only elements that can be acted on. On by default: an agent asking
    /// "where is Save" wants something it can click.
    pub interactive_only: bool,
    pub limit: usize,
    /// A plain-language description of the wanted element ("the button that
    /// saves the document"), ranked by the judge over the candidates the
    /// other filters leave. Needs `[judge]` enabled.
    pub describe: Option<String>,
}

/// One match, with everything needed to decide and then act.
#[derive(Debug, Clone, Serialize)]
pub struct ElementHit {
    #[serde(rename = "ref")]
    pub reff: String,
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    pub secure: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bounds: Option<Bounds>,
    /// Distance in points from `near`, when it was given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance: Option<f64>,
    /// False when the element has no backend handle, so `ui_action` cannot
    /// target it. Reported rather than hidden: an agent that asked for a label
    /// should be told it found a label.
    pub actionable: bool,
    /// The judge's probability that this is the element `describe` meant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probability: Option<f64>,
}

/// Parse the tool arguments. Returns a message suitable for `INVALID_ARGS`.
pub fn parse_query(args: &Value) -> Result<ElementQuery, String> {
    let near = match args.get("near") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let (x, y) = (
                v.get("x").and_then(Value::as_f64),
                v.get("y").and_then(Value::as_f64),
            );
            match (x, y) {
                (Some(x), Some(y)) => Some((x, y)),
                _ => return Err("'near' needs numeric 'x' and 'y'".into()),
            }
        }
    };
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| (n as usize).clamp(1, MAX_LIMIT))
        .unwrap_or(DEFAULT_LIMIT);

    let q = ElementQuery {
        role: args
            .get("role")
            .and_then(Value::as_str)
            .map(|r| normalize_role(r.trim())),
        name: args
            .get("name")
            .and_then(Value::as_str)
            .map(|n| n.trim().to_lowercase())
            .filter(|n| !n.is_empty()),
        near,
        interactive_only: args
            .get("interactive_only")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        limit,
        describe: args
            .get("describe")
            .and_then(Value::as_str)
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty()),
    };
    if q.role.is_none() && q.name.is_none() && q.near.is_none() && q.describe.is_none() {
        return Err("need at least one of 'role', 'name', 'near' or 'describe'".into());
    }
    Ok(q)
}

/// Distance from a point to a rectangle's centre.
///
/// Centre rather than nearest edge: a query like "the button near where I just
/// clicked" is asking which control is *there*, and a large container whose
/// edge happens to be close is not the answer.
fn distance(b: &Bounds, x: f64, y: f64) -> f64 {
    let cx = b.x + b.w / 2.0;
    let cy = b.y + b.h / 2.0;
    ((cx - x).powi(2) + (cy - y).powi(2)).sqrt()
}

fn matches(info: &ElementInfo, q: &ElementQuery) -> bool {
    if let Some(role) = &q.role {
        if &info.role != role {
            return false;
        }
    }
    if let Some(name) = &q.name {
        let hay = info.name.as_deref().unwrap_or_default().to_lowercase();
        if !hay.contains(name) {
            return false;
        }
    }
    if q.interactive_only && info.node_id.is_none() {
        return false;
    }
    true
}

/// Numeric part of a `@eN` ref, for stable tree-order sorting. Refs are
/// allocated in document order, so this is the order the agent would have read.
fn ref_order(reff: &str) -> u64 {
    reff.trim_start_matches("@e").parse().unwrap_or(u64::MAX)
}

/// Run a query against an installed snapshot.
///
/// Returns the hits (already limited) and how many matched in total, so the
/// caller can say "20 of 47" rather than implying it found everything.
pub fn query_snapshot(snap: &Snapshot, q: &ElementQuery) -> (Vec<ElementHit>, usize) {
    let mut hits: Vec<ElementHit> = snap
        .elements
        .iter()
        .filter(|(_, info)| matches(info, q))
        .map(|(reff, info)| ElementHit {
            reff: reff.clone(),
            role: info.role.clone(),
            name: info.name.clone(),
            value: info.value_preview.clone(),
            secure: info.secure,
            bounds: info.bounds,
            distance: q
                .near
                .and_then(|(x, y)| info.bounds.as_ref().map(|b| distance(b, x, y))),
            actionable: info.node_id.is_some(),
            probability: None,
        })
        .collect();

    let total = hits.len();
    if q.near.is_some() {
        // Nearest first; anything without bounds cannot be ranked, so it sorts
        // last rather than being dropped.
        hits.sort_by(|a, b| {
            let da = a.distance.unwrap_or(f64::INFINITY);
            let db = b.distance.unwrap_or(f64::INFINITY);
            da.partial_cmp(&db)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| ref_order(&a.reff).cmp(&ref_order(&b.reff)))
        });
    } else {
        hits.sort_by_key(|h| ref_order(&h.reff));
    }
    // A described query keeps every candidate for the judge to rank; the
    // limit applies after ranking.
    if q.describe.is_none() {
        hits.truncate(q.limit);
    }
    (hits, total)
}

/// How many candidates a described query hands to the judge. Above this the
/// tree-order prefix is what gets ranked, and the response says so.
pub const MAX_DESCRIBE_CANDIDATES: usize = mcp_judge::MAX_CHOICE_OPTIONS;

/// One line per candidate, the way the judge sees it. Secure fields never
/// carry their value.
pub fn candidate_line(h: &ElementHit, window: Option<&str>) -> String {
    let mut s = h.role.to_string();
    if let Some(n) = &h.name {
        s.push_str(&format!(" named {n:?}"));
    }
    if h.secure {
        s.push_str(" (password field)");
    } else if let Some(v) = &h.value {
        let v: String = v.chars().take(80).collect();
        s.push_str(&format!(" with value {v:?}"));
    }
    if let Some(b) = &h.bounds {
        s.push_str(&format!(
            " at ({:.0}, {:.0}) size {:.0}x{:.0}",
            b.x, b.y, b.w, b.h
        ));
    }
    if !h.actionable {
        s.push_str(" (not actionable)");
    }
    if let Some(w) = window {
        s.push_str(&format!(" in window {w:?}"));
    }
    s
}

/// Attach probabilities to hits and order them best first, then cut to the
/// limit. Hits the ranking did not mention keep no probability and sort
/// last, in tree order.
pub fn apply_ranking(
    mut hits: Vec<ElementHit>,
    probabilities: &std::collections::BTreeMap<String, f64>,
    limit: usize,
) -> Vec<ElementHit> {
    for h in hits.iter_mut() {
        h.probability = probabilities.get(&h.reff).copied();
    }
    hits.sort_by(|a, b| {
        let pa = a.probability.unwrap_or(-1.0);
        let pb = b.probability.unwrap_or(-1.0);
        pb.partial_cmp(&pa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| ref_order(&a.reff).cmp(&ref_order(&b.reff)))
    });
    hits.truncate(limit);
    hits
}

/// The tool's JSON schema, in the Gemini-safe subset.
pub fn query_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "app": { "type": "string", "description": "target application name" },
            "surface": {
                "type": "string",
                "enum": ["window", "focused", "menu", "menubar", "sheet", "popover", "alert"],
                "description": "which UI surface to search"
            },
            "role": { "type": "string", "description": "element role, e.g. button, textfield, checkbox" },
            "name": { "type": "string", "description": "case-insensitive substring of the element name" },
            "near": {
                "type": "object",
                "description": "rank by distance from this screen point",
                "properties": { "x": { "type": "number" }, "y": { "type": "number" } }
            },
            "interactive_only": {
                "type": "boolean",
                "description": "only elements ui_action can target (default true)"
            },
            "limit": { "type": "integer", "description": "max matches (default 20, cap 200)" },
            "describe": { "type": "string", "description": "plain-language description of the wanted element, e.g. 'the button that saves the document'; candidates are ranked by the judge and each carries a probability (needs the judge enabled)" }
        },
        "required": []
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(reff: &str, role: &str, name: Option<&str>, secure: bool) -> ElementHit {
        ElementHit {
            reff: reff.into(),
            role: role.into(),
            name: name.map(String::from),
            value: Some("hidden-value".into()),
            secure,
            bounds: Some(Bounds {
                x: 10.0,
                y: 20.0,
                w: 30.0,
                h: 40.0,
            }),
            distance: None,
            actionable: true,
            probability: None,
        }
    }

    #[test]
    fn describe_alone_is_a_valid_query_and_keeps_every_candidate() {
        let q = parse_query(&json!({ "describe": "  the save button " })).unwrap();
        assert_eq!(q.describe.as_deref(), Some("the save button"));
        assert!(q.role.is_none() && q.name.is_none());
        assert!(parse_query(&json!({ "describe": "   " })).is_err());
        assert!(parse_query(&json!({ "describe": "" })).is_err());
    }

    #[test]
    fn ranking_orders_by_probability_then_tree_order_and_cuts_to_the_limit() {
        let hits = vec![
            hit("@e1", "button", Some("Cancel"), false),
            hit("@e2", "button", Some("Save"), false),
            hit("@e3", "link", None, false),
        ];
        let mut p = std::collections::BTreeMap::new();
        p.insert("@e2".to_string(), 0.8);
        p.insert("@e1".to_string(), 0.2);
        let ranked = apply_ranking(hits.clone(), &p, 10);
        let order: Vec<&str> = ranked.iter().map(|h| h.reff.as_str()).collect();
        assert_eq!(order, vec!["@e2", "@e1", "@e3"]);
        assert_eq!(ranked[0].probability, Some(0.8));
        assert_eq!(
            ranked[2].probability, None,
            "unranked hits carry no probability"
        );
        assert_eq!(apply_ranking(hits.clone(), &p, 1).len(), 1);
        assert!(apply_ranking(Vec::new(), &p, 5).is_empty());
        // No probabilities at all: tree order, unchanged.
        let none = apply_ranking(hits, &std::collections::BTreeMap::new(), 10);
        assert_eq!(
            none.iter().map(|h| h.reff.as_str()).collect::<Vec<_>>(),
            vec!["@e1", "@e2", "@e3"]
        );
    }

    #[test]
    fn candidate_lines_never_carry_a_secure_value() {
        let line = candidate_line(
            &hit("@e9", "textfield", Some("Password"), true),
            Some("Login"),
        );
        assert!(line.contains("password field"));
        assert!(!line.contains("hidden-value"), "{line}");
        assert!(line.contains("in window \"Login\""));
        let line = candidate_line(&hit("@e9", "textfield", Some("User"), false), None);
        assert!(line.contains("hidden-value"));
        assert!(line.contains("at (10, 20) size 30x40"));
        let mut h = hit("@e1", "statictext", None, false);
        h.actionable = false;
        h.value = None;
        h.bounds = None;
        assert_eq!(candidate_line(&h, None), "statictext (not actionable)");
    }
    use crate::arena::ElementState;
    use std::collections::HashMap;

    fn el(
        role: &str,
        name: Option<&str>,
        node_id: Option<u64>,
        at: Option<(f64, f64)>,
    ) -> ElementInfo {
        ElementInfo {
            role: role.into(),
            name: name.map(str::to_string),
            value_preview: None,
            secure: false,
            bounds: at.map(|(x, y)| Bounds {
                x,
                y,
                w: 10.0,
                h: 10.0,
            }),
            node_id,
            state: ElementState::default(),
            semantic_intent: None,
            bound_state: None,
        }
    }

    fn snapshot(items: &[(&str, ElementInfo)]) -> Snapshot {
        Snapshot {
            id: "s1".into(),
            app: Some("Test".into()),
            window: None,
            skeleton: false,
            elements: items
                .iter()
                .map(|(r, i)| ((*r).to_string(), i.clone()))
                .collect::<HashMap<_, _>>(),
        }
    }

    fn q(args: serde_json::Value) -> ElementQuery {
        parse_query(&args).unwrap()
    }

    #[test]
    fn a_query_must_ask_for_something() {
        assert!(parse_query(&json!({})).is_err());
        assert!(parse_query(&json!({"limit": 5})).is_err());
        assert!(parse_query(&json!({"role": "button"})).is_ok());
    }

    #[test]
    fn near_needs_both_coordinates() {
        assert!(parse_query(&json!({"near": {"x": 1.0}})).is_err());
        assert!(parse_query(&json!({"near": {"x": 1.0, "y": 2.0}})).is_ok());
    }

    #[test]
    fn the_limit_is_clamped_not_trusted() {
        assert_eq!(q(json!({"role":"button","limit": 99999})).limit, MAX_LIMIT);
        assert_eq!(q(json!({"role":"button","limit": 0})).limit, 1);
        assert_eq!(q(json!({"role":"button"})).limit, DEFAULT_LIMIT);
    }

    /// Role synonyms normalise, so a caller need not know the platform's
    /// spelling: AXButton, button and Button are the same query.
    #[test]
    fn roles_are_normalised_on_both_sides() {
        let s = snapshot(&[("@e1", el("button", Some("Save"), Some(1), None))]);
        for spelling in ["button", "Button", "AXButton"] {
            let (hits, _) = query_snapshot(&s, &q(json!({"role": spelling})));
            assert_eq!(hits.len(), 1, "role {spelling:?} should have matched");
        }
    }

    #[test]
    fn names_match_case_insensitively_on_a_substring() {
        let s = snapshot(&[
            ("@e1", el("button", Some("Save As…"), Some(1), None)),
            ("@e2", el("button", Some("Cancel"), Some(2), None)),
        ]);
        let (hits, total) = query_snapshot(&s, &q(json!({"name": "save"})));
        assert_eq!(total, 1);
        assert_eq!(hits[0].reff, "@e1");
    }

    #[test]
    fn results_are_in_tree_order_by_default() {
        let s = snapshot(&[
            ("@e10", el("button", Some("b"), Some(10), None)),
            ("@e2", el("button", Some("a"), Some(2), None)),
        ]);
        let (hits, _) = query_snapshot(&s, &q(json!({"role": "button"})));
        assert_eq!(
            hits.iter().map(|h| h.reff.as_str()).collect::<Vec<_>>(),
            vec!["@e2", "@e10"],
            "refs sort numerically, not lexically"
        );
    }

    #[test]
    fn near_ranks_by_distance_and_reports_it() {
        let s = snapshot(&[
            (
                "@e1",
                el("button", Some("far"), Some(1), Some((500.0, 500.0))),
            ),
            (
                "@e2",
                el("button", Some("near"), Some(2), Some((10.0, 10.0))),
            ),
        ]);
        let (hits, _) = query_snapshot(&s, &q(json!({"role":"button","near":{"x":12.0,"y":12.0}})));
        assert_eq!(hits[0].reff, "@e2");
        assert!(hits[0].distance.unwrap() < hits[1].distance.unwrap());
    }

    /// An element with no bounds cannot be ranked by distance, but dropping it
    /// would silently hide a match the caller asked for.
    #[test]
    fn unpositioned_elements_sort_last_rather_than_vanishing() {
        let s = snapshot(&[
            ("@e1", el("button", Some("nowhere"), Some(1), None)),
            ("@e2", el("button", Some("here"), Some(2), Some((0.0, 0.0)))),
        ]);
        let (hits, total) =
            query_snapshot(&s, &q(json!({"role":"button","near":{"x":0.0,"y":0.0}})));
        assert_eq!(total, 2);
        assert_eq!(hits[0].reff, "@e2");
        assert_eq!(hits[1].reff, "@e1");
        assert!(hits[1].distance.is_none());
    }

    /// The default hides elements nothing can act on; asking for them returns
    /// them marked, so "found a label, not a button" is visible.
    #[test]
    fn interactive_only_is_the_default_and_can_be_turned_off() {
        let s = snapshot(&[
            ("@e1", el("button", Some("Save"), Some(1), None)),
            ("@e2", el("statictext", Some("Saved"), None, None)),
        ]);
        let (hits, _) = query_snapshot(&s, &q(json!({"name": "sav"})));
        assert_eq!(hits.len(), 1);
        assert!(hits[0].actionable);

        let (hits, _) = query_snapshot(&s, &q(json!({"name":"sav","interactive_only": false})));
        assert_eq!(hits.len(), 2);
        assert!(!hits.iter().all(|h| h.actionable));
    }

    #[test]
    fn total_reports_everything_that_matched_not_just_what_was_returned() {
        let items: Vec<(String, ElementInfo)> = (1..=50)
            .map(|i| (format!("@e{i}"), el("button", Some("x"), Some(i), None)))
            .collect();
        let s = Snapshot {
            id: "s1".into(),
            app: None,
            window: None,
            skeleton: false,
            elements: items.into_iter().collect(),
        };
        let (hits, total) = query_snapshot(&s, &q(json!({"role":"button","limit": 5})));
        assert_eq!(hits.len(), 5);
        assert_eq!(total, 50, "the caller must be told what it did not see");
    }
}
