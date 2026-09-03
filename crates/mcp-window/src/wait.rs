//! Waiting for the UI to settle, as something other tools can reuse.
//!
//! Synthetic input is asynchronous. The event goes to the window server and the
//! application processes it on its own run loop, so observing immediately after
//! acting reads the *previous* state. Every reliable GUI automation is
//! therefore act-then-wait, and the waiting half is the same code whether it is
//! called on its own (`wait_for`) or attached to an action (`expect`).
//!
//! Conditions are all-or-nothing: a spec with several must have all of them
//! hold at the same observation, not each at some point.

use std::sync::Arc;
use std::time::{Duration, Instant};

use mcp_a11y::{flatten, A11yBackend, FlattenConfig, RawSnapshot, SnapshotRequest};
use mcp_types::{CallCtx, CancelToken};
use serde_json::{json, Value};

use crate::backend::{WindowBackend, WindowInfo};

/// How often the condition is re-checked.
const POLL_MS: u64 = 150;
/// Ceiling on any wait, so a bad condition cannot hold a session open.
const MAX_TIMEOUT_MS: u64 = 30_000;
/// Default for `wait_for` called on its own.
const DEFAULT_TIMEOUT_MS: u64 = 5_000;
/// Default for a postcondition. Shorter, because the action has already
/// happened: this is settling time, not an open-ended wait for a human.
pub const EXPECT_TIMEOUT_MS: u64 = 3_000;

/// One thing that must become true.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitCondition {
    /// This text appears anywhere in the observed tree.
    Text(String),
    /// A window whose title contains this exists.
    Window(String),
    /// This text is *gone* from the tree — a dialog dismissed, a spinner
    /// finished. Without it, an agent can only wait for things to appear.
    Gone(String),
    /// An element whose line contains this text is focused.
    Focused(String),
}

/// A parsed wait, with its scope and deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitSpec {
    pub conditions: Vec<WaitCondition>,
    pub app: Option<String>,
    pub timeout_ms: u64,
}

/// What a wait ended up doing.
pub struct WaitOutcome {
    pub met: bool,
    pub waited_ms: u64,
    /// The last observation taken while polling. Handing it back means a caller
    /// that also wants to diff does not have to re-snapshot, which would both
    /// cost another traversal and race the next change.
    pub last: Option<RawSnapshot>,
}

/// Parse a `wait_for` argument object or an `expect` clause.
pub fn parse_wait_spec(v: &Value, default_timeout_ms: u64) -> Result<WaitSpec, String> {
    let s = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|x| !x.is_empty())
    };
    let mut conditions = Vec::new();
    // `element` is accepted as a synonym for `text`: an agent that has a ref or
    // a label is asking the same question either way.
    if let Some(t) = s("text").or_else(|| s("element")) {
        conditions.push(WaitCondition::Text(t));
    }
    if let Some(t) = s("window") {
        conditions.push(WaitCondition::Window(t));
    }
    if let Some(t) = s("gone") {
        conditions.push(WaitCondition::Gone(t));
    }
    if let Some(t) = s("focused") {
        conditions.push(WaitCondition::Focused(t));
    }
    if conditions.is_empty() {
        return Err("need at least one of text|element|window|gone|focused".into());
    }
    Ok(WaitSpec {
        conditions,
        app: s("app"),
        timeout_ms: v
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(default_timeout_ms)
            .min(MAX_TIMEOUT_MS),
    })
}

/// Schema for the conditions, shared by `wait_for` and every `expect` clause so
/// the two cannot drift apart.
pub fn wait_schema(description: &str) -> Value {
    json!({
        "type": "object",
        "description": description,
        "properties": {
            "text": { "type": "string", "description": "wait until this text appears in the UI" },
            "element": { "type": "string", "description": "synonym for text" },
            "window": { "type": "string", "description": "wait until a window with this title exists" },
            "gone": { "type": "string", "description": "wait until this text is no longer present" },
            "focused": { "type": "string", "description": "wait until an element matching this text has focus" },
            "app": { "type": "string", "description": "which application to observe" },
            "timeout_ms": { "type": "integer", "description": "give up after this long (max 30000)" }
        },
        "required": []
    })
}

/// Does a text-based condition hold against a flattened tree?
///
/// Pure, so the interesting cases are testable without a desktop.
pub fn text_condition_holds(cond: &WaitCondition, tree: &str) -> bool {
    match cond {
        WaitCondition::Text(t) => tree.contains(t.as_str()),
        WaitCondition::Gone(t) => !tree.contains(t.as_str()),
        // The flattener appends `focused` to the line of the focused element,
        // so "is this thing focused" is a property of that one line.
        WaitCondition::Focused(t) => tree
            .lines()
            .any(|l| l.contains(t.as_str()) && l.split_whitespace().any(|w| w == "focused")),
        WaitCondition::Window(_) => false,
    }
}

/// Does a window condition hold against a window list?
pub fn window_condition_holds(title: &str, windows: &[WindowInfo]) -> bool {
    windows
        .iter()
        .any(|w| w.title.as_deref().is_some_and(|t| t.contains(title)))
}

/// Poll `probe` until it returns true or the deadline passes.
///
/// Separated from any backend so the loop itself — first check before any
/// sleep, honouring cancellation, reporting elapsed time — is testable.
pub async fn poll_until<F, Fut>(
    timeout_ms: u64,
    interval_ms: u64,
    cancel: &CancelToken,
    mut probe: F,
) -> (bool, u64)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = Instant::now();
    loop {
        // Check before sleeping: a condition that is already true must not cost
        // a poll interval, which is most of them after a fast action.
        if cancel.is_cancelled() {
            return (false, start.elapsed().as_millis() as u64);
        }
        if probe().await {
            return (true, start.elapsed().as_millis() as u64);
        }
        let elapsed = start.elapsed().as_millis() as u64;
        if elapsed >= timeout_ms {
            return (false, elapsed);
        }
        let remaining = timeout_ms - elapsed;
        tokio::time::sleep(Duration::from_millis(interval_ms.min(remaining))).await;
    }
}

/// Evaluates a [`WaitSpec`] against the live UI.
#[derive(Clone)]
pub struct WaitEvaluator {
    window: Arc<dyn WindowBackend>,
    a11y: Arc<dyn A11yBackend>,
}

impl WaitEvaluator {
    pub fn new(window: Arc<dyn WindowBackend>, a11y: Arc<dyn A11yBackend>) -> Self {
        WaitEvaluator { window, a11y }
    }

    /// Wait for every condition in `spec` to hold at once.
    pub async fn wait(&self, spec: &WaitSpec, ctx: &CallCtx) -> WaitOutcome {
        let needs_tree = spec
            .conditions
            .iter()
            .any(|c| !matches!(c, WaitCondition::Window(_)));
        let needs_windows = spec
            .conditions
            .iter()
            .any(|c| matches!(c, WaitCondition::Window(_)));

        let last: std::sync::Mutex<Option<RawSnapshot>> = std::sync::Mutex::new(None);
        let (met, waited_ms) = poll_until(spec.timeout_ms, POLL_MS, &ctx.cancel, || async {
            // One observation per poll, shared by every text condition: three
            // conditions must not mean three traversals of the same tree.
            let tree = if needs_tree {
                let req = SnapshotRequest {
                    app: spec.app.clone(),
                    ..Default::default()
                };
                match self.a11y.snapshot(&req).await {
                    Ok(raw) => {
                        let f = flatten(
                            &raw.root,
                            raw.app.as_deref(),
                            raw.window.as_deref(),
                            "wait",
                            &FlattenConfig {
                                // No budget: a condition must not fail because
                                // the text it waits for fell off the end.
                                max_chars: usize::MAX,
                                terminal_app: raw.terminal_app,
                                ..FlattenConfig::default()
                            },
                        );
                        *last.lock().unwrap_or_else(|e| e.into_inner()) = Some(raw);
                        Some(f.text)
                    }
                    Err(_) => None,
                }
            } else {
                None
            };
            let windows = if needs_windows {
                self.window.list_windows(spec.app.as_deref()).await.ok()
            } else {
                None
            };
            spec.conditions.iter().all(|c| match c {
                WaitCondition::Window(t) => windows
                    .as_deref()
                    .is_some_and(|ws| window_condition_holds(t, ws)),
                other => tree
                    .as_deref()
                    .is_some_and(|t| text_condition_holds(other, t)),
            })
        })
        .await;

        WaitOutcome {
            met,
            waited_ms,
            last: last.into_inner().unwrap_or(None),
        }
    }
}

/// The default timeout for a standalone `wait_for`.
pub fn default_wait_timeout() -> u64 {
    DEFAULT_TIMEOUT_MS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spec_must_contain_a_condition() {
        assert!(parse_wait_spec(&json!({}), 1000).is_err());
        assert!(parse_wait_spec(&json!({"app": "TextEdit"}), 1000).is_err());
        assert!(parse_wait_spec(&json!({"text": "hi"}), 1000).is_ok());
    }

    #[test]
    fn element_is_a_synonym_for_text() {
        let a = parse_wait_spec(&json!({"text": "Save"}), 1000).unwrap();
        let b = parse_wait_spec(&json!({"element": "Save"}), 1000).unwrap();
        assert_eq!(a.conditions, b.conditions);
    }

    #[test]
    fn several_conditions_are_all_required() {
        let s = parse_wait_spec(&json!({"text": "a", "window": "b", "gone": "c"}), 1000).unwrap();
        assert_eq!(s.conditions.len(), 3);
    }

    #[test]
    fn the_timeout_is_capped() {
        let s = parse_wait_spec(&json!({"text": "a", "timeout_ms": 10_000_000}), 1000).unwrap();
        assert_eq!(s.timeout_ms, MAX_TIMEOUT_MS);
        let s = parse_wait_spec(&json!({"text": "a"}), 1234).unwrap();
        assert_eq!(s.timeout_ms, 1234);
    }

    const TREE: &str = "snapshot s1 app=\"TextEdit\"\n\
                        @e1 textarea value=\"hello\" focused\n\
                        @e2 button \"Save\"\n\
                        @e3 button \"Cancel\" disabled";

    #[test]
    fn text_appears_and_disappears() {
        assert!(text_condition_holds(
            &WaitCondition::Text("Save".into()),
            TREE
        ));
        assert!(!text_condition_holds(
            &WaitCondition::Text("Publish".into()),
            TREE
        ));
        // `gone` is the condition an agent needs to wait for a dialog to close
        // or a spinner to finish, and it is the exact inverse.
        assert!(text_condition_holds(
            &WaitCondition::Gone("Publish".into()),
            TREE
        ));
        assert!(!text_condition_holds(
            &WaitCondition::Gone("Save".into()),
            TREE
        ));
    }

    /// Focus is a property of one line, not of the tree: "Save" appearing
    /// somewhere does not mean Save has focus.
    #[test]
    fn focus_is_matched_on_the_elements_own_line() {
        assert!(text_condition_holds(
            &WaitCondition::Focused("textarea".into()),
            TREE
        ));
        assert!(!text_condition_holds(
            &WaitCondition::Focused("Save".into()),
            TREE
        ));
    }

    /// A word that merely contains "focused" is not the focus flag.
    #[test]
    fn focus_matching_does_not_fire_on_a_substring() {
        let tree = "@e1 button \"unfocused-thing\"";
        assert!(!text_condition_holds(
            &WaitCondition::Focused("button".into()),
            tree
        ));
    }

    #[test]
    fn window_titles_match_on_a_substring() {
        let ws = vec![WindowInfo {
            id: 0,
            app: Some("TextEdit".into()),
            title: Some("Untitled 4".into()),
            bounds: None,
            minimized: false,
        }];
        assert!(window_condition_holds("Untitled", &ws));
        assert!(!window_condition_holds("Report", &ws));
        assert!(!window_condition_holds("Untitled", &[]));
    }

    #[tokio::test]
    async fn a_condition_already_true_costs_no_wait() {
        let cancel = CancelToken::new();
        let (met, waited) = poll_until(5_000, 500, &cancel, || async { true }).await;
        assert!(met);
        assert!(waited < 100, "must not sleep before the first check");
    }

    #[tokio::test]
    async fn polling_stops_at_the_deadline_and_reports_elapsed() {
        let cancel = CancelToken::new();
        let (met, waited) = poll_until(200, 20, &cancel, || async { false }).await;
        assert!(!met);
        assert!(waited >= 200, "waited {waited}ms");
    }

    #[tokio::test]
    async fn a_condition_that_becomes_true_is_caught() {
        let cancel = CancelToken::new();
        let n = std::cell::Cell::new(0);
        let (met, _) = poll_until(5_000, 10, &cancel, || {
            n.set(n.get() + 1);
            let hit = n.get() >= 3;
            async move { hit }
        })
        .await;
        assert!(met);
        assert_eq!(n.get(), 3);
    }

    /// The kill switch cancels in-flight work, so a wait must not outlive it.
    #[tokio::test]
    async fn cancellation_ends_the_wait() {
        let cancel = CancelToken::new();
        cancel.cancel();
        let (met, waited) = poll_until(30_000, 100, &cancel, || async { false }).await;
        assert!(!met);
        assert!(waited < 100);
    }
}
