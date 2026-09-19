use std::sync::Arc;

use async_trait::async_trait;
use mcp_types::{CallCtx, Category, Envelope, ErrorCode, Tier, ToolDescriptor, ToolModule};
use serde_json::{json, Value};

use crate::store::{Recipe, Step, Store, StoreError};

pub struct MemoryModule {
    store: Store,
    max_results: usize,
    judge: Option<Arc<mcp_judge::Judge>>,
}

impl MemoryModule {
    pub fn new(store: Store) -> Self {
        MemoryModule {
            store,
            max_results: 10,
            judge: None,
        }
    }

    /// Attach the judge that reranks `memory_find` hits against the goal when
    /// the caller asks for it (`rerank: true`).
    pub fn with_judge(mut self, judge: Arc<mcp_judge::Judge>) -> Self {
        self.judge = Some(judge);
        self
    }

    /// Reorder the substring matches by how well each recipe's *goal* fits the
    /// request, rather than by success count alone. Read-only: it changes the
    /// order of the returned list, never what is stored. Degrades to the
    /// deterministic order (and says so) when the judge cannot answer, because
    /// success-count order is itself useful.
    async fn find_reranked(&self, goal: &str, hits: Vec<Recipe>) -> Envelope {
        let tool = "memory_find";
        if goal.trim().is_empty() {
            return Envelope::fail_with(
                tool,
                ErrorCode::InvalidArgs,
                "rerank needs a 'goal' to rank the recipes against",
                "pass the goal you want recipes for, or drop rerank to list by success count",
            );
        }
        let Some(judge) = self.judge.as_ref().filter(|j| j.enabled()) else {
            return Envelope::fail_with(
                tool,
                ErrorCode::UnsupportedOs,
                "rerank needs the judge, which is not enabled",
                "set [judge] enabled = \"true\" and provide TYPESAFE_API_KEY, or drop rerank",
            );
        };
        if hits.len() < 2 {
            // Nothing to reorder; the single (or empty) result stands.
            return Envelope::ok(
                tool,
                json!({
                    "recipes": hits, "count": hits.len(),
                    "reranked": false, "reason": "fewer than two matches to reorder"
                }),
            );
        }
        let candidates = recipe_candidates(&hits);
        let state = json!({ "request": goal, "candidates": candidates });
        match judge
            .rank(
                state,
                "Which candidate recipe in `candidates` best achieves `request`? Judge by how closely each recipe's goal matches the request, not by how many steps it has.",
                &candidates,
            )
            .await
        {
            Ok(r) => {
                let hits = reorder_by_probability(hits, &r.probabilities);
                Envelope::ok(
                    tool,
                    json!({
                        "recipes": hits, "count": hits.len(), "reranked": true,
                        "ranking": {
                            "best": r.choice,
                            "confidence": r.confidence,
                            "any_fits": r.any_fits,
                            "confident": r.any_fits >= judge.match_threshold(),
                        }
                    }),
                )
            }
            Err(e) => {
                tracing::debug!(error = %e.message(), "memory rerank skipped; success-count order stands");
                Envelope::ok(
                    tool,
                    json!({
                        "recipes": hits, "count": hits.len(),
                        "reranked": false, "reason": e.message()
                    }),
                )
            }
        }
    }
}

/// Describe each recipe for the judge, keyed by its (unique) normalized goal.
fn recipe_candidates(hits: &[Recipe]) -> std::collections::BTreeMap<String, String> {
    hits.iter()
        .map(|r| {
            (
                r.goal_norm.clone(),
                format!(
                    "Goal \"{}\" ({} step(s), succeeded {} time(s))",
                    r.goal,
                    r.steps.len(),
                    r.success_count
                ),
            )
        })
        .collect()
}

/// Stable-sort the hits by the probability the judge gave each recipe's
/// normalized goal, most likely first. A recipe the judge did not score sinks
/// to the bottom rather than jumping the queue.
fn reorder_by_probability(
    mut hits: Vec<Recipe>,
    probabilities: &std::collections::BTreeMap<String, f64>,
) -> Vec<Recipe> {
    hits.sort_by(|a, b| {
        let pa = probabilities.get(&a.goal_norm).copied().unwrap_or(-1.0);
        let pb = probabilities.get(&b.goal_norm).copied().unwrap_or(-1.0);
        pb.partial_cmp(&pa).unwrap_or(std::cmp::Ordering::Equal)
    });
    hits
}

fn err(tool: &str, e: StoreError) -> Envelope {
    match e {
        StoreError::Invalid(m) => Envelope::fail(tool, ErrorCode::InvalidArgs, m),
        StoreError::Io(m) => Envelope::fail(tool, ErrorCode::ActionFailed, m),
    }
}

#[async_trait]
impl ToolModule for MemoryModule {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        vec![
            ToolDescriptor::new(
                "memory_save",
                Category::Memory,
                Tier::Standard,
                "Record a sequence that achieved a goal, so a later run can replay it. Steps hold \
                 selectors ({role,name,app,window?,index?}), never element refs: refs belong to \
                 one snapshot. Saving an existing goal replaces its steps and counts the success.",
                json!({
                    "type": "object",
                    "properties": {
                        "goal": { "type": "string" },
                        "steps": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "tool": { "type": "string" },
                                    "args": { "type": "object" },
                                    "selector": {
                                        "type": "object",
                                        "properties": {
                                            "role": { "type": "string" },
                                            "name": { "type": "string" },
                                            "app": { "type": "string" },
                                            "window": { "type": "string" },
                                            "index": { "type": "integer" }
                                        },
                                        "required": ["role", "app"]
                                    }
                                },
                                "required": ["tool"]
                            }
                        },
                        "evidence": { "type": "string", "description": "how success was confirmed" }
                    },
                    "required": ["goal", "steps"]
                }),
            ),
            ToolDescriptor::new(
                "memory_find",
                Category::Memory,
                Tier::Read,
                "Look up recorded sequences for a goal. Exact (normalized) match wins; otherwise \
                 substring matches ranked by success count. Omit 'goal' to list everything. \
                 Set 'rerank' to reorder the matches by how well each recipe's goal fits yours, \
                 judged semantically (needs the judge enabled); the order and a 'ranking' block \
                 are reported, and it falls back to success-count order if the judge is \
                 unavailable.",
                json!({"type":"object","properties":{
                    "goal":{"type":"string"},
                    "limit":{"type":"integer"},
                    "rerank":{"type":"boolean","description":"reorder matches by semantic fit to 'goal' using the judge"}},"required":[]}),
            ),
            ToolDescriptor::new(
                "memory_forget",
                Category::Memory,
                Tier::Standard,
                "Delete the recorded sequence for a goal.",
                json!({"type":"object","properties":{"goal":{"type":"string"}},"required":["goal"]}),
            ),
        ]
    }

    async fn call(&self, name: &str, args: Value, _ctx: &CallCtx) -> Envelope {
        match name {
            "memory_save" => {
                let tool = "memory_save";
                let Some(goal) = args.get("goal").and_then(Value::as_str) else {
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'goal'");
                };
                let Some(raw) = args.get("steps").and_then(Value::as_array) else {
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'steps' array");
                };
                let mut steps = Vec::with_capacity(raw.len());
                for (i, v) in raw.iter().enumerate() {
                    match serde_json::from_value::<Step>(v.clone()) {
                        Ok(s) => steps.push(s),
                        Err(e) => {
                            return Envelope::fail(
                                tool,
                                ErrorCode::InvalidArgs,
                                format!("step {i}: {e}"),
                            )
                        }
                    }
                }
                let evidence = args
                    .get("evidence")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                match self.store.save(goal, steps, evidence, mcp_policy::now_ms()) {
                    Ok(r) => Envelope::ok(
                        tool,
                        json!({
                            "goal": r.goal, "goal_norm": r.goal_norm,
                            "steps": r.steps.len(), "success_count": r.success_count
                        }),
                    ),
                    Err(e) => err(tool, e),
                }
            }
            "memory_find" => {
                let goal = args.get("goal").and_then(Value::as_str).unwrap_or("");
                let limit = args
                    .get("limit")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize)
                    .unwrap_or(self.max_results)
                    .clamp(1, self.max_results);
                let rerank = args.get("rerank").and_then(Value::as_bool).unwrap_or(false);
                let hits = match self.store.find(goal, limit) {
                    Ok(hits) => hits,
                    Err(e) => return err("memory_find", e),
                };
                if !rerank {
                    return Envelope::ok(
                        "memory_find",
                        json!({ "recipes": hits, "count": hits.len() }),
                    );
                }
                self.find_reranked(goal, hits).await
            }
            "memory_forget" => {
                let Some(goal) = args.get("goal").and_then(Value::as_str) else {
                    return Envelope::fail(
                        "memory_forget",
                        ErrorCode::InvalidArgs,
                        "missing 'goal'",
                    );
                };
                match self.store.forget(goal) {
                    Ok(removed) => Envelope::ok("memory_forget", json!({ "forgotten": removed })),
                    Err(e) => err("memory_forget", e),
                }
            }
            other => Envelope::fail(other, ErrorCode::InvalidArgs, "unknown tool"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_types::CancelToken;

    fn module(tag: &str) -> MemoryModule {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "agentctl-recall-tool-{tag}-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&p);
        MemoryModule::new(Store::new(p, 50, 20))
    }
    fn ctx() -> CallCtx {
        CallCtx::new("t", CancelToken::new())
    }

    #[tokio::test]
    async fn save_find_forget_round_trip() {
        let m = module("roundtrip");
        let steps = json!([{
            "tool": "ui_action",
            "selector": { "role": "button", "name": "Send", "app": "Mail" },
            "args": { "action": "press" }
        }]);
        let saved = m
            .call(
                "memory_save",
                json!({ "goal": "Send the draft", "steps": steps }),
                &ctx(),
            )
            .await;
        assert!(saved.ok, "{saved:?}");
        assert_eq!(saved.data.unwrap()["success_count"], 1);

        let found = m
            .call("memory_find", json!({ "goal": "send the draft" }), &ctx())
            .await;
        let d = found.data.unwrap();
        assert_eq!(d["count"], 1);
        assert_eq!(d["recipes"][0]["steps"][0]["selector"]["name"], "Send");

        let gone = m
            .call("memory_forget", json!({ "goal": "Send the draft" }), &ctx())
            .await;
        assert_eq!(gone.data.unwrap()["forgotten"], true);
        let after = m
            .call("memory_find", json!({ "goal": "send the draft" }), &ctx())
            .await;
        assert_eq!(after.data.unwrap()["count"], 0);
    }

    /// A ref is snapshot-scoped; storing one would replay against a dead
    /// handle. The schema only accepts selectors, and a malformed one is a
    /// clear per-step error rather than a silently dropped field.
    #[tokio::test]
    async fn malformed_steps_report_which_step_failed() {
        let m = module("malformed");
        let env = m
            .call(
                "memory_save",
                json!({ "goal": "g", "steps": [{ "tool": "ui_action" }, { "nope": 1 }] }),
                &ctx(),
            )
            .await;
        assert!(!env.ok);
        let e = env.error.unwrap();
        assert_eq!(e.code, ErrorCode::InvalidArgs);
        assert!(e.message.contains("step 1"), "{}", e.message);
    }

    #[tokio::test]
    async fn find_with_no_goal_lists_everything_ranked() {
        let m = module("listall");
        for (g, n) in [("alpha", 1), ("beta", 3)] {
            for _ in 0..n {
                m.call(
                    "memory_save",
                    json!({ "goal": g, "steps": [{ "tool": "exec",
                        "selector": { "role": "button", "name": "X", "app": "A" } }] }),
                    &ctx(),
                )
                .await;
            }
        }
        let all = m.call("memory_find", json!({}), &ctx()).await.data.unwrap();
        assert_eq!(all["count"], 2);
        assert_eq!(all["recipes"][0]["goal"], "beta", "ranked by success count");
    }

    #[tokio::test]
    async fn nothing_here_needs_consent() {
        let m = module("consent");
        for t in ["memory_save", "memory_find", "memory_forget"] {
            assert!(m.consent_prompt(t, &json!({})).is_none());
        }
    }

    // ---- rerank ---------------------------------------------------------

    use std::sync::Mutex;
    use std::time::Duration;

    /// A transport that replays one scripted reply.
    struct OneReply(Mutex<Option<Result<(u16, String), String>>>);
    #[async_trait]
    impl mcp_judge::Transport for OneReply {
        async fn post(
            &self,
            _u: &str,
            _k: &str,
            _b: &Value,
            _t: Duration,
        ) -> Result<(u16, String), String> {
            self.0
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| Err("exhausted".into()))
        }
    }

    fn judge(reply: Result<(u16, String), String>) -> Arc<mcp_judge::Judge> {
        let cfg = mcp_judge::JudgeConfig {
            enabled: true,
            match_threshold: Some(0.6),
            ..mcp_judge::JudgeConfig::default()
        };
        Arc::new(mcp_judge::Judge::with_transport(
            cfg,
            Some("k".into()),
            Box::new(OneReply(Mutex::new(Some(reply)))),
        ))
    }

    /// Save two recipes that share the substring "restart the" (so a lookup by
    /// it returns both) but whose success counts and semantic fit disagree:
    /// "restart the machine" has more successes, while "restart the app" is the
    /// better semantic match for the app-focused requests below.
    async fn two_recipes(tag: &str, judge: Option<Arc<mcp_judge::Judge>>) -> MemoryModule {
        let mut m = module(tag);
        if let Some(j) = judge {
            m = m.with_judge(j);
        }
        for _ in 0..3 {
            m.call(
                "memory_save",
                json!({ "goal": "restart the machine", "steps": [{ "tool": "exec",
                    "selector": { "role": "button", "name": "X", "app": "A" } }] }),
                &ctx(),
            )
            .await;
        }
        m.call(
            "memory_save",
            json!({ "goal": "restart the app", "steps": [{ "tool": "exec",
                "selector": { "role": "button", "name": "X", "app": "A" } }] }),
            &ctx(),
        )
        .await;
        m
    }

    #[tokio::test]
    async fn without_rerank_the_order_is_success_count_and_no_judge_is_asked() {
        // A judge that would error if consulted proves rerank=false never asks.
        let m = two_recipes("norerank", Some(judge(Err("must not be called".into())))).await;
        let out = m
            .call("memory_find", json!({ "goal": "restart the" }), &ctx())
            .await;
        assert!(out.ok, "{out:?}");
        let data = out.data.unwrap();
        assert_eq!(data["count"], 2, "both recipes match the shared substring");
        assert_eq!(data["recipes"][0]["goal"], "restart the machine");
        assert!(data.get("reranked").is_none());
    }

    #[tokio::test]
    async fn rerank_reorders_by_semantic_fit_over_success_count() {
        // The judge favours "restart the app" though "machine" has more wins.
        let body = r#"{"answers":{
            "pick":{"type":"choice","choice":"restart the app","probabilities":{"restart the app":0.85,"restart the machine":0.15},"confidence":0.85},
            "any":{"type":"noul","noul":0.9}
        }}"#;
        let m = two_recipes("rerank", Some(judge(Ok((200, body.into()))))).await;
        let out = m
            .call(
                "memory_find",
                json!({ "goal": "restart the", "rerank": true }),
                &ctx(),
            )
            .await;
        assert!(out.ok, "{out:?}");
        let data = out.data.unwrap();
        assert_eq!(data["reranked"], true);
        assert_eq!(
            data["recipes"][0]["goal"], "restart the app",
            "semantic fit beats success count once reranked"
        );
        assert_eq!(data["ranking"]["confident"], true);
    }

    #[tokio::test]
    async fn rerank_degrades_to_success_count_order_when_the_judge_fails() {
        let m = two_recipes("degrade", Some(judge(Ok((500, "boom".into()))))).await;
        let out = m
            .call(
                "memory_find",
                json!({ "goal": "restart the", "rerank": true }),
                &ctx(),
            )
            .await;
        assert!(out.ok, "a judge failure must not fail the lookup: {out:?}");
        let data = out.data.unwrap();
        assert_eq!(data["reranked"], false);
        assert_eq!(
            data["recipes"][0]["goal"], "restart the machine",
            "deterministic success-count order still stands"
        );
    }

    #[tokio::test]
    async fn rerank_without_the_judge_enabled_is_a_clear_error() {
        let m = two_recipes("noneenabled", None).await;
        let out = m
            .call(
                "memory_find",
                json!({ "goal": "restart the", "rerank": true }),
                &ctx(),
            )
            .await;
        assert!(!out.ok);
        let msg = out.error.unwrap().message;
        assert!(msg.contains("judge"), "{msg}");
    }
}
