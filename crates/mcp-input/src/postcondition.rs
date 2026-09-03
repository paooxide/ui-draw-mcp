//! Postconditions: acting and confirming in one call.
//!
//! An agent that wants to know whether a click worked has to observe, act, wait
//! and observe again — four round trips to a remote model, of which three carry
//! no decision. Worse, the naive version is *wrong*: synthetic input is
//! asynchronous, so observing straight after acting reads the previous state.
//!
//! An `expect` clause folds all four into one. The action runs, the condition
//! is waited for using the same evaluator `wait_for` uses, and the result
//! carries the delta against the snapshot taken before the action — so the
//! answer is not "the call returned ok" but "here is what changed".
//!
//! When the condition is *not* met the delta is returned anyway, on the error.
//! The action still happened, so what it did is exactly what the agent needs to
//! see; reporting only "timed out" throws that away.

use std::sync::{Arc, Mutex};

use mcp_a11y::{
    diff_snapshots, flatten, A11yBackend, FlattenConfig, Snapshot, SnapshotArena, SnapshotRequest,
};
use mcp_types::{CallCtx, Envelope, ErrorCode};
use mcp_window::{parse_wait_spec, WaitEvaluator, WaitSpec, EXPECT_TIMEOUT_MS};
use serde_json::{json, Value};

/// Runs an `expect` clause: wait, then diff.
pub struct Verifier {
    evaluator: WaitEvaluator,
    a11y: Arc<dyn A11yBackend>,
    arena: Arc<Mutex<SnapshotArena>>,
}

/// What verification found.
pub struct Verified {
    pub met: bool,
    pub waited_ms: u64,
    pub delta: Value,
    pub snapshot_id: Option<String>,
}

impl Verifier {
    pub fn new(
        evaluator: WaitEvaluator,
        a11y: Arc<dyn A11yBackend>,
        arena: Arc<Mutex<SnapshotArena>>,
    ) -> Self {
        Verifier {
            evaluator,
            a11y,
            arena,
        }
    }

    /// Parse an `expect` clause, if the caller supplied one.
    ///
    /// Returns the tool's own error envelope on a bad clause, so a malformed
    /// postcondition is refused *before* the action runs rather than after.
    pub fn parse(tool: &str, args: &Value) -> Result<Option<WaitSpec>, Box<Envelope>> {
        match args.get("expect") {
            None | Some(Value::Null) => Ok(None),
            Some(v) if !v.is_object() => Err(Box::new(Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                "'expect' must be an object",
            ))),
            Some(v) => parse_wait_spec(v, EXPECT_TIMEOUT_MS)
                .map(Some)
                .map_err(|m| {
                    Box::new(Envelope::fail(
                        tool,
                        ErrorCode::InvalidArgs,
                        format!("'expect': {m}"),
                    ))
                }),
        }
    }

    /// The current snapshot, cloned before the action so there is something to
    /// diff against afterwards.
    pub fn before(&self) -> Option<Snapshot> {
        self.arena
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .current()
            .cloned()
    }

    /// Wait for the condition, then describe what changed.
    pub async fn verify(
        &self,
        spec: &WaitSpec,
        before: Option<Snapshot>,
        ctx: &CallCtx,
    ) -> Verified {
        let outcome = self.evaluator.wait(spec, ctx).await;

        // Reuse the evaluator's last observation where possible: taking another
        // costs a second traversal and races whatever changes next.
        let raw = match outcome.last {
            Some(r) => Some(r),
            None => {
                let req = SnapshotRequest {
                    app: spec
                        .app
                        .clone()
                        .or_else(|| before.as_ref().and_then(|b| b.app.clone())),
                    ..Default::default()
                };
                self.a11y.snapshot(&req).await.ok()
            }
        };

        let Some(raw) = raw else {
            return Verified {
                met: outcome.met,
                waited_ms: outcome.waited_ms,
                delta: json!({ "unavailable": "could not observe the UI after the action" }),
                snapshot_id: None,
            };
        };

        let sid = {
            let mut arena = self.arena.lock().unwrap_or_else(|e| e.into_inner());
            arena.next_id()
        };
        // No character budget: a truncated element map would drop changes from
        // the diff, and the text is not returned here anyway.
        let f = flatten(
            &raw.root,
            raw.app.as_deref(),
            raw.window.as_deref(),
            &sid,
            &FlattenConfig {
                max_chars: usize::MAX,
                terminal_app: raw.terminal_app,
                ..FlattenConfig::default()
            },
        );
        let after = f.snapshot;
        let delta = match &before {
            Some(b) => diff_snapshots(b, &after).to_json(),
            None => json!({
                "unavailable": "no snapshot was taken before the action; call get_ui_tree first"
            }),
        };
        {
            // Installing makes the refs inside the delta actionable, which is
            // the point of returning them at all.
            let mut arena = self.arena.lock().unwrap_or_else(|e| e.into_inner());
            arena.install(after);
        }
        Verified {
            met: outcome.met,
            waited_ms: outcome.waited_ms,
            delta,
            snapshot_id: Some(sid),
        }
    }
}

/// Attach a verification result to a tool's envelope.
pub fn attach(env: Envelope, tool: &str, v: Verified) -> Envelope {
    if !env.ok {
        // The action itself failed; the postcondition never got a chance and
        // reporting on it would obscure the real error.
        return env;
    }
    let mut data = env.data.unwrap_or_else(|| json!({}));
    if let Some(obj) = data.as_object_mut() {
        obj.insert(
            "expect".into(),
            json!({ "met": v.met, "waited_ms": v.waited_ms }),
        );
        obj.insert("delta".into(), v.delta.clone());
        if let Some(sid) = &v.snapshot_id {
            obj.insert("snapshot_id".into(), json!(sid));
        }
    }
    if v.met {
        Envelope::ok(tool, data)
    } else {
        Envelope::fail_with_data(
            tool,
            ErrorCode::Timeout,
            format!(
                "the action ran, but the expectation was not met after {}ms",
                v.waited_ms
            ),
            "inspect 'delta' for what did change, then re-observe with get_ui_tree",
            data,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verified(met: bool) -> Verified {
        Verified {
            met,
            waited_ms: 42,
            delta: json!({"counts": {"changed": 1}}),
            snapshot_id: Some("s7".into()),
        }
    }

    #[test]
    fn no_expect_clause_is_not_an_error() {
        assert!(Verifier::parse("ui_action", &json!({"ref": "@e1"}))
            .unwrap()
            .is_none());
        assert!(Verifier::parse("ui_action", &json!({"expect": null}))
            .unwrap()
            .is_none());
    }

    /// A malformed clause is refused before the action runs: discovering the
    /// expectation was nonsense *after* clicking is too late.
    #[test]
    fn a_malformed_expect_clause_is_rejected() {
        let e = *Verifier::parse("ui_action", &json!({"expect": "soon"})).unwrap_err();
        assert_eq!(e.error.unwrap().code, ErrorCode::InvalidArgs);
        let e = *Verifier::parse("ui_action", &json!({"expect": {}})).unwrap_err();
        assert!(e.error.unwrap().message.contains("expect"));
    }

    #[test]
    fn a_valid_clause_defaults_to_the_postcondition_timeout() {
        let spec = Verifier::parse("ui_action", &json!({"expect": {"text": "Saved"}}))
            .unwrap()
            .unwrap();
        assert_eq!(spec.timeout_ms, EXPECT_TIMEOUT_MS);
    }

    #[test]
    fn a_met_expectation_keeps_the_tools_own_result_and_adds_the_delta() {
        let env = Envelope::ok("ui_action", json!({"ok": true}));
        let out = attach(env, "ui_action", verified(true));
        assert!(out.ok);
        let d = out.data.unwrap();
        assert_eq!(d["ok"], json!(true), "the tool's own fields survive");
        assert_eq!(d["expect"]["met"], json!(true));
        assert_eq!(d["expect"]["waited_ms"], json!(42));
        assert_eq!(d["delta"]["counts"]["changed"], json!(1));
        assert_eq!(d["snapshot_id"], json!("s7"));
    }

    /// The action ran, so what it did is exactly what the agent needs to see.
    /// An error that carries only "timed out" throws that away.
    #[test]
    fn an_unmet_expectation_still_carries_the_delta() {
        let env = Envelope::ok("keyboard_type", json!({"ok": true, "typed": 5}));
        let out = attach(env, "keyboard_type", verified(false));
        assert!(!out.ok);
        assert_eq!(out.error.as_ref().unwrap().code, ErrorCode::Timeout);
        let d = out.data.expect("the delta must survive the failure");
        assert_eq!(d["expect"]["met"], json!(false));
        assert_eq!(d["delta"]["counts"]["changed"], json!(1));
        assert_eq!(d["typed"], json!(5));
    }

    /// If the action failed there is nothing to verify, and reporting on the
    /// postcondition would bury the real error.
    #[test]
    fn a_failed_action_is_reported_as_itself() {
        let env = Envelope::fail("ui_action", ErrorCode::StaleRef, "ref is stale");
        let out = attach(env, "ui_action", verified(false));
        assert_eq!(out.error.unwrap().code, ErrorCode::StaleRef);
    }
}
