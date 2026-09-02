//! The recall store (`docs/planning.md` §5.9).
//!
//! A JSON file, not SQLite. The plan named SQLite; a store holding a few
//! hundred recorded sequences does not need a query engine, and pulling in a
//! bundled SQLite would cost several megabytes of build output against a
//! standing disk constraint. The semantics the plan actually specified —
//! `goal_norm` uniqueness, replace-on-resave, `success_count` — are kept.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How to find a UI element again on a later run.
///
/// Deliberately *not* an element ref: refs belong to one snapshot of one
/// session, so a replayed ref points at nothing (or worse, at something else).
/// A selector is re-resolved against a fresh tree.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Selector {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub app: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    /// Position among same-role siblings; the tiebreaker for unnamed controls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<u32>,
}

impl Selector {
    /// Can this selector identify one element on its own?
    ///
    /// An unnamed control ("the third button") is ambiguous unless something
    /// narrows it. Storing an ambiguous selector is worse than storing nothing:
    /// replay would confidently click the wrong control.
    pub fn is_addressable(&self) -> bool {
        if self.role.trim().is_empty() || self.app.trim().is_empty() {
            return false;
        }
        let named = self.name.as_deref().is_some_and(|n| !n.trim().is_empty());
        named || self.window.is_some() || self.index.is_some()
    }

    /// Identity for within-goal uniqueness.
    pub fn key(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}",
            self.app,
            self.window.as_deref().unwrap_or(""),
            self.role,
            self.name.as_deref().unwrap_or(""),
            self.index.map(|i| i.to_string()).unwrap_or_default()
        )
    }
}

/// One recorded tool call.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Step {
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<Selector>,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub args: serde_json::Value,
}

/// A goal and the sequence that achieved it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Recipe {
    pub goal: String,
    /// Normalized goal — the uniqueness key.
    pub goal_norm: String,
    pub steps: Vec<Step>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    pub success_count: u32,
    pub updated_ms: u128,
}

/// Lowercase, collapse whitespace, drop trailing punctuation. "Open the Inbox"
/// and "open  the inbox." are the same goal.
pub fn normalize_goal(goal: &str) -> String {
    let lowered = goal.to_lowercase();
    let collapsed: Vec<&str> = lowered.split_whitespace().collect();
    collapsed
        .join(" ")
        .trim_end_matches(['.', '!', '?', ':', ';'])
        .to_string()
}

#[derive(Debug)]
pub enum StoreError {
    Invalid(String),
    Io(String),
}

/// File-backed recipe store, keyed by normalized goal.
pub struct Store {
    path: PathBuf,
    max_recipes: usize,
    max_steps: usize,
}

impl Store {
    pub fn new(path: PathBuf, max_recipes: usize, max_steps: usize) -> Self {
        Store {
            path,
            max_recipes,
            max_steps,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load(&self) -> Result<BTreeMap<String, Recipe>, StoreError> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| {
                StoreError::Io(format!(
                    "{}: unreadable recall store: {e}",
                    self.path.display()
                ))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(StoreError::Io(format!("{}: {e}", self.path.display()))),
        }
    }

    fn persist(&self, map: &BTreeMap<String, Recipe>) -> Result<(), StoreError> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| StoreError::Io(e.to_string()))?;
        }
        let text = serde_json::to_string_pretty(map).map_err(|e| StoreError::Io(e.to_string()))?;
        // Write-then-rename: an interrupted save must not leave a truncated
        // store that fails to parse on the next start.
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, text).map_err(|e| StoreError::Io(e.to_string()))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| StoreError::Io(e.to_string()))
    }

    /// Insert or replace. Re-saving an existing goal **replaces** its steps and
    /// bumps `success_count` — a later successful run supersedes an earlier one
    /// rather than accumulating stale variants.
    pub fn save(
        &self,
        goal: &str,
        steps: Vec<Step>,
        evidence: Option<String>,
        now_ms: u128,
    ) -> Result<Recipe, StoreError> {
        let goal = goal.trim();
        if goal.is_empty() {
            return Err(StoreError::Invalid("'goal' must not be empty".into()));
        }
        if steps.is_empty() {
            return Err(StoreError::Invalid("'steps' must not be empty".into()));
        }
        if steps.len() > self.max_steps {
            return Err(StoreError::Invalid(format!(
                "{} steps exceeds the {} step limit",
                steps.len(),
                self.max_steps
            )));
        }
        let mut seen = std::collections::HashSet::new();
        for (i, s) in steps.iter().enumerate() {
            if s.tool.trim().is_empty() {
                return Err(StoreError::Invalid(format!("step {i} has no 'tool'")));
            }
            if let Some(sel) = &s.selector {
                if !sel.is_addressable() {
                    return Err(StoreError::Invalid(format!(
                        "step {i}: selector cannot identify one element — an unnamed control needs \
                         a 'window' or 'index' tiebreaker, or replay would act on the wrong one"
                    )));
                }
                if !seen.insert(sel.key()) {
                    return Err(StoreError::Invalid(format!(
                        "step {i}: selector '{}' is not unique within this recipe",
                        sel.key()
                    )));
                }
            }
        }

        let goal_norm = normalize_goal(goal);
        let mut map = self.load()?;
        let success_count = map.get(&goal_norm).map(|r| r.success_count).unwrap_or(0) + 1;
        if !map.contains_key(&goal_norm) && map.len() >= self.max_recipes {
            return Err(StoreError::Invalid(format!(
                "recall store is full ({} recipes); forget one first",
                self.max_recipes
            )));
        }
        let recipe = Recipe {
            goal: goal.to_string(),
            goal_norm: goal_norm.clone(),
            steps,
            evidence,
            success_count,
            updated_ms: now_ms,
        };
        map.insert(goal_norm, recipe.clone());
        self.persist(&map)?;
        Ok(recipe)
    }

    /// Exact normalized match first, then substring, ranked by success count.
    pub fn find(&self, goal: &str, limit: usize) -> Result<Vec<Recipe>, StoreError> {
        let norm = normalize_goal(goal);
        let map = self.load()?;
        if let Some(hit) = map.get(&norm) {
            return Ok(vec![hit.clone()]);
        }
        if norm.is_empty() {
            let mut all: Vec<Recipe> = map.into_values().collect();
            all.sort_by_key(|r| std::cmp::Reverse(r.success_count));
            all.truncate(limit);
            return Ok(all);
        }
        let mut hits: Vec<Recipe> = map
            .into_values()
            .filter(|r| r.goal_norm.contains(&norm) || norm.contains(&r.goal_norm))
            .collect();
        hits.sort_by(|a, b| {
            b.success_count
                .cmp(&a.success_count)
                .then(b.updated_ms.cmp(&a.updated_ms))
        });
        hits.truncate(limit);
        Ok(hits)
    }

    pub fn forget(&self, goal: &str) -> Result<bool, StoreError> {
        let norm = normalize_goal(goal);
        let mut map = self.load()?;
        let removed = map.remove(&norm).is_some();
        if removed {
            self.persist(&map)?;
        }
        Ok(removed)
    }

    pub fn count(&self) -> Result<usize, StoreError> {
        Ok(self.load()?.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_store(tag: &str) -> Store {
        let mut p = std::env::temp_dir();
        p.push(format!("agentctl-recall-{tag}-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&p);
        Store::new(p, 100, 50)
    }

    fn sel(name: Option<&str>) -> Selector {
        Selector {
            role: "button".into(),
            name: name.map(str::to_string),
            app: "Mail".into(),
            window: None,
            index: None,
        }
    }

    fn step(name: Option<&str>) -> Step {
        Step {
            tool: "ui_action".into(),
            selector: Some(sel(name)),
            args: serde_json::json!({ "action": "press" }),
        }
    }

    #[test]
    fn goals_normalize_to_one_key() {
        assert_eq!(normalize_goal("Open the Inbox."), "open the inbox");
        assert_eq!(normalize_goal("open   the  inbox"), "open the inbox");
        assert_eq!(normalize_goal("  OPEN THE INBOX!  "), "open the inbox");
    }

    /// v1's bug was an `ON CONFLICT` that never updated. Re-saving a goal must
    /// replace the steps and count the success, not silently keep the old ones.
    #[test]
    fn resaving_a_goal_replaces_steps_and_counts_the_success() {
        let s = tmp_store("replace");
        let first = s
            .save("Open the inbox", vec![step(Some("Inbox"))], None, 1)
            .unwrap();
        assert_eq!(first.success_count, 1);

        let second = s
            .save(
                "open the inbox.",
                vec![step(Some("All Mail"))],
                Some("ok".into()),
                2,
            )
            .unwrap();
        assert_eq!(second.success_count, 2, "success must accumulate");
        assert_eq!(
            second.steps,
            vec![step(Some("All Mail"))],
            "steps must be replaced"
        );
        assert_eq!(s.count().unwrap(), 1, "normalized goal is the unique key");
    }

    /// An unnamed control is ambiguous; replaying it would press whatever
    /// happens to sit in that role. Refuse to store it without a tiebreaker.
    #[test]
    fn ambiguous_selectors_are_refused() {
        let s = tmp_store("ambig");
        let err = s.save("do a thing", vec![step(None)], None, 1).unwrap_err();
        assert!(matches!(err, StoreError::Invalid(m) if m.contains("tiebreaker")));

        let mut ok = step(None);
        ok.selector.as_mut().unwrap().index = Some(2);
        assert!(
            s.save("do a thing", vec![ok], None, 1).is_ok(),
            "index disambiguates"
        );

        let mut win = step(None);
        win.selector.as_mut().unwrap().window = Some("Compose".into());
        assert!(
            s.save("other thing", vec![win], None, 1).is_ok(),
            "window disambiguates"
        );
    }

    #[test]
    fn duplicate_selectors_within_one_recipe_are_refused() {
        let s = tmp_store("dupe");
        let err = s
            .save(
                "twice",
                vec![step(Some("Send")), step(Some("Send"))],
                None,
                1,
            )
            .unwrap_err();
        assert!(matches!(err, StoreError::Invalid(m) if m.contains("not unique")));
    }

    #[test]
    fn find_prefers_exact_then_falls_back_to_substring() {
        let s = tmp_store("find");
        s.save("open the inbox", vec![step(Some("Inbox"))], None, 1)
            .unwrap();
        s.save(
            "open the inbox and archive",
            vec![step(Some("Archive"))],
            None,
            2,
        )
        .unwrap();

        let exact = s.find("Open the inbox!", 5).unwrap();
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].goal_norm, "open the inbox");

        let fuzzy = s.find("inbox", 5).unwrap();
        assert_eq!(fuzzy.len(), 2, "substring match finds both");
    }

    #[test]
    fn forget_removes_only_the_named_goal() {
        let s = tmp_store("forget");
        s.save("a goal", vec![step(Some("X"))], None, 1).unwrap();
        s.save("b goal", vec![step(Some("Y"))], None, 1).unwrap();
        assert!(s.forget("A GOAL.").unwrap());
        assert!(!s.forget("A GOAL.").unwrap(), "second forget is a no-op");
        assert_eq!(s.count().unwrap(), 1);
    }

    /// A truncated or corrupt store must surface as an error, not silently
    /// present as "no memories" and let the agent overwrite what was there.
    #[test]
    fn a_corrupt_store_is_an_error_not_an_empty_one() {
        let s = tmp_store("corrupt");
        std::fs::write(s.path(), "{not json").unwrap();
        assert!(matches!(s.find("anything", 5), Err(StoreError::Io(_))));
        assert!(matches!(s.count(), Err(StoreError::Io(_))));
    }

    #[test]
    fn limits_are_enforced() {
        let mut p = std::env::temp_dir();
        p.push(format!("agentctl-recall-lim-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let s = Store::new(p, 2, 3);
        assert!(
            s.save("empty", vec![], None, 1).is_err(),
            "no empty recipes"
        );
        let many: Vec<Step> = (0..4)
            .map(|i| {
                let mut st = step(Some("B"));
                st.selector.as_mut().unwrap().index = Some(i);
                st
            })
            .collect();
        assert!(s.save("too many", many, None, 1).is_err());
        s.save("one", vec![step(Some("A"))], None, 1).unwrap();
        s.save("two", vec![step(Some("B"))], None, 1).unwrap();
        assert!(
            s.save("three", vec![step(Some("C"))], None, 1).is_err(),
            "store is full"
        );
        // An existing goal can still be updated when full.
        assert!(s.save("one", vec![step(Some("A2"))], None, 2).is_ok());
    }
}
