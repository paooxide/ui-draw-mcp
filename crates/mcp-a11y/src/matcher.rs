//! Finding an element by what it is called.
//!
//! One matcher serves `find_elements` and every input tool that accepts a
//! `name`, so "the Save button" resolves to the same element whichever tool
//! asks. A hash map's iteration order must never decide the winner: the
//! snapshot's elements live in one, and the first match it happened to yield
//! used to be the one acted on.
//!
//! Ranking, best first: exact name, then prefix, then substring (so `Save`
//! beats `Save As…`); a name hit beats a value hit; an interactive control
//! beats anything else; then document order, which is the `@eN` number.

use serde::Serialize;
use serde_json::Value;

use crate::arena::{ElementInfo, Snapshot};
use crate::tree::{is_interactive_role, normalize_role};

/// Lowercase and collapse runs of whitespace, so `"Save  As\n"` and
/// `"save as"` compare equal.
pub fn fold(s: &str) -> String {
    s.split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// A ref in any of the spellings a caller produces: `@e12`, `e12`, `12`, or
/// the JSON number 12. `None` when it is none of those.
pub fn parse_ref(v: &Value) -> Option<String> {
    let digits = match v {
        Value::Number(n) => n.as_u64()?.to_string(),
        Value::String(s) => {
            let t = s.trim();
            let t = t.strip_prefix('@').unwrap_or(t);
            let t = t
                .strip_prefix('e')
                .or_else(|| t.strip_prefix('E'))
                .unwrap_or(t);
            if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            t.to_string()
        }
        _ => return None,
    };
    // `@e007` and `@e7` are the same ref.
    let n: u64 = digits.parse().ok()?;
    Some(format!("@e{n}"))
}

/// Numeric part of a `@eN` ref. Refs are allocated in document order, so this
/// is the order the agent would have read the tree in.
pub fn ref_order(reff: &str) -> u64 {
    reff.trim_start_matches("@e").parse().unwrap_or(u64::MAX)
}

/// Role spellings that mean the same control. A caller says `popup button`,
/// the macOS tree says `popupbutton`, AT-SPI says `combobox`.
const ROLE_GROUPS: &[&[&str]] = &[
    &["button", "pushbutton"],
    &[
        "popupbutton",
        "combobox",
        "menubutton",
        "popup",
        "dropdown",
        "select",
    ],
    &[
        "textfield",
        "textarea",
        "textbox",
        "entry",
        "edit",
        "input",
        "searchfield",
    ],
    &["checkbox", "check"],
    &["radiobutton", "radio"],
    &["link", "hyperlink"],
    &["menuitem"],
    &["slider"],
    &["tab", "pagetab", "tabitem"],
];

/// A role reduced to letters and digits, without the platform prefix:
/// `AXPopUpButton`, `pop up button` and `popup_button` all become `popupbutton`.
fn role_key(role: &str) -> String {
    let squashed: String = role
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_ascii_lowercase();
    // `normalize_role` strips the AX prefix, but only before squashing could
    // have joined it to the word after it.
    normalize_role(&squashed)
}

/// Does an element of role `have` satisfy a request for role `want`?
pub fn role_matches(have: &str, want: &str) -> bool {
    let (h, w) = (role_key(have), role_key(want));
    if h == w {
        return true;
    }
    ROLE_GROUPS
        .iter()
        .any(|g| g.contains(&h.as_str()) && g.contains(&w.as_str()))
}

/// How well a name matched. Lower is better, and the derived order is the
/// ranking: quality, then which field, then how control-like the element is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Score {
    /// 0 exact, 1 prefix, 2 substring.
    pub quality: u8,
    /// 0 matched the name, 1 matched only the value.
    pub field: u8,
    /// 0 interactive control, 1 anything else.
    pub tier: u8,
}

fn quality(hay: &str, needle: &str) -> Option<u8> {
    if hay == needle {
        Some(0)
    } else if hay.starts_with(needle) {
        Some(1)
    } else if hay.contains(needle) {
        Some(2)
    } else {
        None
    }
}

/// Score one element against an already-folded query, or `None` for no match.
/// The name is the element's title or, failing that, its description; the
/// value is what a text field or popup currently shows.
pub fn score(info: &ElementInfo, folded: &str) -> Option<Score> {
    if folded.is_empty() {
        return None;
    }
    let by_name = info
        .name
        .as_deref()
        .and_then(|n| quality(&fold(n), folded))
        .map(|q| (q, 0u8));
    // A secure field never carries a value, so there is nothing to match.
    let by_value = info
        .value_preview
        .as_deref()
        .and_then(|v| quality(&fold(v), folded))
        .map(|q| (q, 1u8));
    let (q, field) = [by_name, by_value].into_iter().flatten().min()?;
    let tier = u8::from(!(is_interactive_role(&info.role) && info.node_id.is_some()));
    Some(Score {
        quality: q,
        field,
        tier,
    })
}

/// One ranked match.
#[derive(Debug, Clone)]
pub struct RankedMatch<'a> {
    pub reff: &'a str,
    pub info: &'a ElementInfo,
    pub score: Score,
}

/// Every element matching `name` (and `role`, with synonyms) that passes
/// `usable`, best first. Deterministic: ties break on document order.
pub fn rank_by_name<'a>(
    snap: &'a Snapshot,
    name: &str,
    role: Option<&str>,
    usable: &dyn Fn(&ElementInfo) -> bool,
) -> Vec<RankedMatch<'a>> {
    let folded = fold(name);
    let mut out: Vec<RankedMatch<'a>> = snap
        .elements
        .iter()
        .filter(|(_, info)| usable(info))
        .filter(|(_, info)| match role {
            Some(r) => role_matches(&info.role, r),
            None => true,
        })
        .filter_map(|(reff, info)| {
            score(info, &folded).map(|score| RankedMatch {
                reff: reff.as_str(),
                info,
                score,
            })
        })
        .collect();
    out.sort_by(|a, b| {
        a.score
            .cmp(&b.score)
            .then_with(|| ref_order(a.reff).cmp(&ref_order(b.reff)))
            .then_with(|| a.reff.cmp(b.reff))
    });
    out
}

/// How many leading matches are as good as the first. More than one means the
/// name did not pick a single element and the caller deserves to hear it.
pub fn tied_with_best(ranked: &[RankedMatch<'_>]) -> usize {
    match ranked.first() {
        Some(best) => ranked.iter().take_while(|m| m.score == best.score).count(),
        None => 0,
    }
}

/// An element offered when a name matched nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Candidate {
    #[serde(rename = "ref")]
    pub reff: String,
    pub role: String,
    pub name: String,
}

impl Candidate {
    /// `@e3 button "Save"`, the way a snapshot line reads.
    pub fn line(&self) -> String {
        format!("{} {} {:?}", self.reff, self.role, self.name)
    }
}

fn words(s: &str) -> Vec<String> {
    fold(s)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// Up to `limit` elements whose names share a word with `name`: the likely
/// intent when an exact lookup failed. Most shared words first, controls
/// before anything else, then document order.
pub fn near_misses(
    snap: &Snapshot,
    name: &str,
    usable: &dyn Fn(&ElementInfo) -> bool,
    limit: usize,
) -> Vec<Candidate> {
    let wanted = words(name);
    if wanted.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(usize, u8, u64, &str, &ElementInfo)> = snap
        .elements
        .iter()
        .filter(|(_, info)| usable(info))
        .filter_map(|(reff, info)| {
            let have = words(info.name.as_deref()?);
            let shared = wanted.iter().filter(|w| have.contains(w)).count();
            (shared > 0).then(|| {
                let tier = u8::from(!is_interactive_role(&info.role));
                (shared, tier, ref_order(reff), reff.as_str(), info)
            })
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(a.1.cmp(&b.1))
            .then(a.2.cmp(&b.2))
            .then(a.3.cmp(b.3))
    });
    scored
        .into_iter()
        .take(limit)
        .map(|(_, _, _, reff, info)| Candidate {
            reff: reff.to_string(),
            role: info.role.clone(),
            name: info.name.clone().unwrap_or_default(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn el(role: &str, name: Option<&str>, value: Option<&str>, node: Option<u64>) -> ElementInfo {
        ElementInfo {
            role: role.into(),
            name: name.map(str::to_string),
            value_preview: value.map(str::to_string),
            secure: false,
            bounds: None,
            node_id: node,
            state: Default::default(),
            semantic_intent: None,
            bound_state: None,
        }
    }

    fn snap(items: Vec<(&str, ElementInfo)>) -> Snapshot {
        Snapshot {
            id: "s1".into(),
            app: None,
            window: None,
            skeleton: false,
            elements: items
                .into_iter()
                .map(|(r, i)| (r.to_string(), i))
                .collect::<HashMap<_, _>>(),
        }
    }

    fn refs(r: &[RankedMatch<'_>]) -> Vec<String> {
        r.iter().map(|m| m.reff.to_string()).collect()
    }

    const ANY: &dyn Fn(&ElementInfo) -> bool = &|_| true;

    #[test]
    fn refs_parse_in_every_spelling() {
        for good in [
            json!("@e12"),
            json!("e12"),
            json!("12"),
            json!(12),
            json!(" @E12 "),
        ] {
            assert_eq!(parse_ref(&good).as_deref(), Some("@e12"), "{good}");
        }
        assert_eq!(parse_ref(&json!("@e007")).as_deref(), Some("@e7"));
        for bad in [
            json!("@e"),
            json!("button"),
            json!(""),
            json!(-1),
            json!(1.5),
            json!(null),
            json!("@e1x"),
        ] {
            assert_eq!(parse_ref(&bad), None, "{bad}");
        }
    }

    #[test]
    fn folding_ignores_case_and_whitespace_runs() {
        assert_eq!(fold("  Save \n As\t"), "save as");
    }

    #[test]
    fn exact_beats_prefix_beats_substring() {
        let s = snap(vec![
            ("@e1", el("button", Some("Save As…"), None, Some(1))),
            ("@e2", el("button", Some("Autosave"), None, Some(2))),
            ("@e3", el("button", Some("Save"), None, Some(3))),
        ]);
        let r = rank_by_name(&s, "save", None, ANY);
        assert_eq!(refs(&r), ["@e3", "@e1", "@e2"]);
        assert_eq!(tied_with_best(&r), 1);
    }

    #[test]
    fn matching_is_case_insensitive_and_whitespace_collapsed() {
        let s = snap(vec![(
            "@e1",
            el("button", Some("Save   As"), None, Some(1)),
        )]);
        assert_eq!(rank_by_name(&s, " SAVE as ", None, ANY).len(), 1);
    }

    #[test]
    fn a_name_hit_beats_a_value_hit_of_the_same_quality() {
        let s = snap(vec![
            ("@e1", el("textfield", Some("Title"), Some("Save"), Some(1))),
            ("@e2", el("button", Some("Save"), None, Some(2))),
        ]);
        assert_eq!(refs(&rank_by_name(&s, "save", None, ANY)), ["@e2", "@e1"]);
    }

    #[test]
    fn a_value_alone_can_be_matched() {
        let s = snap(vec![(
            "@e1",
            el("textfield", None, Some("alice@example.com"), Some(1)),
        )]);
        assert_eq!(refs(&rank_by_name(&s, "alice@example", None, ANY)), ["@e1"]);
    }

    #[test]
    fn controls_outrank_other_elements_at_equal_quality() {
        let s = snap(vec![
            ("@e1", el("group", Some("Save"), None, Some(1))),
            ("@e2", el("button", Some("Save"), None, Some(2))),
        ]);
        assert_eq!(refs(&rank_by_name(&s, "save", None, ANY)), ["@e2", "@e1"]);
    }

    /// The old lookup walked a `HashMap`, so which of two equal matches won
    /// changed from run to run. Ties now go to document order, numerically.
    #[test]
    fn ties_resolve_to_document_order_every_time() {
        let items: Vec<(String, ElementInfo)> = (1..=40)
            .map(|i| (format!("@e{i}"), el("button", Some("OK"), None, Some(i))))
            .collect();
        for _ in 0..20 {
            let s = Snapshot {
                id: "s".into(),
                app: None,
                window: None,
                skeleton: false,
                elements: items.iter().cloned().collect(),
            };
            let r = rank_by_name(&s, "ok", None, ANY);
            assert_eq!(r[0].reff, "@e1");
            assert_eq!(r[1].reff, "@e2");
            assert_eq!(r[9].reff, "@e10", "refs order numerically, not lexically");
            assert_eq!(tied_with_best(&r), 40);
        }
    }

    #[test]
    fn role_filters_use_synonyms() {
        let s = snap(vec![
            ("@e1", el("popupbutton", Some("Format"), None, Some(1))),
            ("@e2", el("button", Some("Format"), None, Some(2))),
        ]);
        for want in [
            "popup button",
            "PopUpButton",
            "AXPopUpButton",
            "combobox",
            "menu button",
        ] {
            assert_eq!(
                refs(&rank_by_name(&s, "format", Some(want), ANY)),
                ["@e1"],
                "{want}"
            );
        }
        assert_eq!(
            refs(&rank_by_name(&s, "format", Some("push button"), ANY)),
            ["@e2"]
        );
    }

    #[test]
    fn role_synonym_table() {
        for (a, b) in [
            ("button", "pushbutton"),
            ("popupbutton", "combobox"),
            ("combobox", "menubutton"),
            ("textfield", "text field"),
            ("textfield", "textarea"),
            ("textfield", "entry"),
            ("textfield", "edit"),
            ("checkbox", "check box"),
            ("radiobutton", "radio button"),
            ("radiobutton", "radio"),
            ("link", "hyperlink"),
            ("menuitem", "menu item"),
            ("tab", "page tab"),
            ("slider", "Slider"),
        ] {
            assert!(role_matches(a, b) && role_matches(b, a), "{a} ~ {b}");
        }
        assert!(!role_matches("button", "link"));
        assert!(!role_matches("checkbox", "radiobutton"));
    }

    #[test]
    fn elements_the_caller_cannot_use_are_skipped() {
        let s = snap(vec![
            ("@e1", el("button", Some("Save"), None, None)),
            ("@e2", el("button", Some("Save"), None, Some(2))),
        ]);
        let has_handle: &dyn Fn(&ElementInfo) -> bool = &|i| i.node_id.is_some();
        assert_eq!(refs(&rank_by_name(&s, "save", None, has_handle)), ["@e2"]);
    }

    #[test]
    fn near_misses_share_a_word_and_are_capped() {
        let mut items = vec![
            (
                "@e1".to_string(),
                el("button", Some("Save Draft"), None, Some(1)),
            ),
            (
                "@e2".to_string(),
                el("button", Some("Cancel"), None, Some(2)),
            ),
            (
                "@e3".to_string(),
                el("link", Some("Save and Close"), None, Some(3)),
            ),
            (
                "@e4".to_string(),
                el("button", Some("Save the Date"), None, Some(4)),
            ),
        ];
        for i in 5..=12 {
            items.push((
                format!("@e{i}"),
                el("button", Some("Save changes now"), None, Some(i)),
            ));
        }
        let s = Snapshot {
            id: "s".into(),
            app: None,
            window: None,
            skeleton: false,
            elements: items.into_iter().collect(),
        };
        let c = near_misses(&s, "Save changes", ANY, 5);
        assert_eq!(c.len(), 5);
        // Two shared words outrank one; the rest keep document order.
        assert_eq!(c[0].reff, "@e5");
        assert!(c.iter().all(|c| c.name != "Cancel"));
        assert_eq!(c[0].line(), "@e5 button \"Save changes now\"");
        assert!(near_misses(&s, "zzz", ANY, 5).is_empty());
        assert!(near_misses(&s, "   ", ANY, 5).is_empty());
    }
}
