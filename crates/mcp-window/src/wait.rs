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
    /// A plain-language claim about the UI ("the document has been saved")
    /// that the judge finds true of the observed tree at or above its
    /// threshold. Needs `[judge]` enabled; without it the wait fails at
    /// parse time rather than pretending.
    Judged(String),
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
    /// The judge's last probability for a `judge` condition, when one was
    /// asked, whether or not it cleared the threshold.
    pub judge_probability: Option<f64>,
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
    if let Some(t) = s("judge") {
        conditions.push(WaitCondition::Judged(t));
    }
    if conditions.is_empty() {
        return Err("need at least one of text|element|window|gone|focused|judge".into());
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
            "judge": { "type": "string", "description": "wait until this plain-language claim about the UI is judged true (needs the judge enabled); the probability is reported" },
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
        WaitCondition::Window(_) | WaitCondition::Judged(_) => false,
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
    let deadline = start + Duration::from_millis(timeout_ms);
    loop {
        // Check before sleeping: a condition that is already true must not cost
        // a poll interval, which is most of them after a fast action.
        if cancel.is_cancelled() {
            return (false, start.elapsed().as_millis() as u64);
        }
        // The probe itself is raced against the deadline. Observing a busy
        // application can take seconds, and checking the clock only *after* the
        // probe returns lets a single slow observation overrun the timeout by
        // an order of magnitude — measured at 12.7s against a stated 1s while
        // snapshotting Finder. A cap that a slow probe can ignore is not a cap.
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return (false, start.elapsed().as_millis() as u64);
        }
        match tokio::time::timeout(remaining, probe()).await {
            Ok(true) => return (true, start.elapsed().as_millis() as u64),
            Ok(false) => {}
            Err(_) => return (false, start.elapsed().as_millis() as u64),
        }
        let elapsed = start.elapsed().as_millis() as u64;
        if elapsed >= timeout_ms {
            return (false, elapsed);
        }
        let left = timeout_ms - elapsed;
        tokio::time::sleep(Duration::from_millis(interval_ms.min(left))).await;
    }
}

/// Evaluates a [`WaitSpec`] against the live UI.
#[derive(Clone)]
pub struct WaitEvaluator {
    window: Arc<dyn WindowBackend>,
    a11y: Arc<dyn A11yBackend>,
    judge: Option<Arc<mcp_judge::Judge>>,
}

/// Ask the judge whether `claim` holds of `tree`. `None` when it could not
/// answer, which never counts as met.
pub async fn judged_claim_holds(judge: &mcp_judge::Judge, claim: &str, tree: &str) -> Option<f64> {
    let state = json!({ "claim": claim, "ui": judge.fit(tree) });
    judge
        .noul(
            state,
            "`ui` is the accessibility tree of an application's window as text, one element per line. Is `claim` true of the state that tree shows right now?",
            "Yes: the tree shows the state the claim describes",
            "No: the tree does not show it, or shows the opposite, or shows too little to tell",
        )
        .await
        .ok()
}

impl WaitEvaluator {
    pub fn new(window: Arc<dyn WindowBackend>, a11y: Arc<dyn A11yBackend>) -> Self {
        WaitEvaluator {
            window,
            a11y,
            judge: None,
        }
    }

    /// Attach the judge that evaluates `judge` conditions.
    pub fn with_judge(mut self, judge: Arc<mcp_judge::Judge>) -> Self {
        self.judge = Some(judge);
        self
    }

    /// Whether this evaluator can honour a `judge` condition.
    pub fn judge_available(&self) -> bool {
        self.judge.as_ref().is_some_and(|j| j.enabled())
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
        let last_probability: std::sync::Mutex<Option<f64>> = std::sync::Mutex::new(None);
        let started = Instant::now();
        let describe = describe(spec);
        let (met, waited_ms) = poll_until(spec.timeout_ms, POLL_MS, &ctx.cancel, || async {
            // A wait is the one place this server is deliberately slow, so it
            // is the one place silence is ambiguous between "working" and
            // "hung". Costs nothing unless the client asked for reports.
            ctx.progress(
                started.elapsed().as_millis() as f64,
                Some(spec.timeout_ms as f64),
                Some(&describe),
            );
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
            // The structural conditions first; the judge is asked only when
            // they all hold, so a wait never pays for a judgment it will
            // not use, and a judgment can never stand in for a structural
            // condition that failed.
            let structural = spec.conditions.iter().all(|c| match c {
                WaitCondition::Window(t) => windows
                    .as_deref()
                    .is_some_and(|ws| window_condition_holds(t, ws)),
                WaitCondition::Judged(_) => true,
                other => tree
                    .as_deref()
                    .is_some_and(|t| text_condition_holds(other, t)),
            });
            if !structural {
                return false;
            }
            let mut judged_ok = true;
            for c in &spec.conditions {
                let WaitCondition::Judged(claim) = c else {
                    continue;
                };
                let (Some(j), Some(t)) = (self.judge.as_ref(), tree.as_deref()) else {
                    return false;
                };
                match judged_claim_holds(j, claim, t).await {
                    Some(p) => {
                        *last_probability.lock().unwrap_or_else(|e| e.into_inner()) = Some(p);
                        if p < j.threshold() {
                            judged_ok = false;
                        }
                    }
                    None => judged_ok = false,
                }
            }
            judged_ok
        })
        .await;

        WaitOutcome {
            met,
            waited_ms,
            judge_probability: last_probability.into_inner().unwrap_or(None),
            last: last.into_inner().unwrap_or(None),
        }
    }
}

/// A short human-readable form of what is being waited for.
fn describe(spec: &WaitSpec) -> String {
    let parts: Vec<String> = spec
        .conditions
        .iter()
        .map(|c| match c {
            WaitCondition::Text(t) => format!("text {t:?}"),
            WaitCondition::Window(t) => format!("window {t:?}"),
            WaitCondition::Gone(t) => format!("{t:?} gone"),
            WaitCondition::Focused(t) => format!("{t:?} focused"),
            WaitCondition::Judged(t) => format!("judged {t:?}"),
        })
        .collect();
    format!("waiting for {}", parts.join(" and "))
}

/// The default timeout for a standalone `wait_for`.
pub fn default_wait_timeout() -> u64 {
    DEFAULT_TIMEOUT_MS
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scripted(std::sync::Mutex<Vec<Result<(u16, String), String>>>);

    #[async_trait::async_trait]
    impl mcp_judge::Transport for Scripted {
        async fn post(
            &self,
            _u: &str,
            _k: &str,
            _b: &Value,
            _t: Duration,
        ) -> Result<(u16, String), String> {
            let mut r = self.0.lock().unwrap();
            if r.is_empty() {
                Err("exhausted".into())
            } else {
                r.remove(0)
            }
        }
    }

    fn judge_saying(p: f64) -> mcp_judge::Judge {
        let reply = format!(r#"{{"answers":{{"q":{{"type":"noul","noul":{p}}}}}}}"#);
        mcp_judge::Judge::with_transport(
            mcp_judge::JudgeConfig {
                enabled: true,
                ..mcp_judge::JudgeConfig::default()
            },
            Some("k".into()),
            Box::new(Scripted(std::sync::Mutex::new(vec![Ok((200, reply))]))),
        )
    }

    #[test]
    fn a_judge_condition_parses_and_is_never_a_text_condition() {
        let s = parse_wait_spec(&json!({ "judge": "the file is saved" }), 1000).unwrap();
        assert_eq!(
            s.conditions,
            vec![WaitCondition::Judged("the file is saved".into())]
        );
        assert!(!text_condition_holds(
            &WaitCondition::Judged("x".into()),
            "x"
        ));
        assert!(describe(&s).contains("judged"));
        assert!(parse_wait_spec(&json!({ "judge": "" }), 1000).is_err());
    }

    #[tokio::test]
    async fn a_judged_claim_reports_its_probability_or_nothing() {
        let j = judge_saying(0.83);
        assert_eq!(
            judged_claim_holds(&j, "saved", "window \"x\"\n button \"Save\"").await,
            Some(0.83)
        );
        let down = mcp_judge::Judge::with_transport(
            mcp_judge::JudgeConfig {
                enabled: true,
                ..mcp_judge::JudgeConfig::default()
            },
            Some("k".into()),
            Box::new(Scripted(std::sync::Mutex::new(vec![
                Err("down".into()),
                Err("down".into()),
            ]))),
        );
        assert_eq!(judged_claim_holds(&down, "saved", "tree").await, None);
        let off = mcp_judge::Judge::with_transport(
            mcp_judge::JudgeConfig::default(),
            None,
            Box::new(Scripted(std::sync::Mutex::new(vec![]))),
        );
        assert_eq!(judged_claim_holds(&off, "saved", "tree").await, None);
    }

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
    /// A probe that takes longer than the whole timeout must not be allowed to
    /// run to completion: the deadline is a deadline.
    #[tokio::test]
    async fn a_slow_probe_cannot_overrun_the_deadline() {
        let cancel = CancelToken::new();
        let started = Instant::now();
        let (met, waited) = poll_until(150, 20, &cancel, || async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            true
        })
        .await;
        assert!(!met);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a 150ms wait must not take {waited}ms"
        );
    }

    #[tokio::test]
    async fn cancellation_ends_the_wait() {
        let cancel = CancelToken::new();
        cancel.cancel();
        let (met, waited) = poll_until(30_000, 100, &cancel, || async { false }).await;
        assert!(!met);
        assert!(waited < 100);
    }
}
