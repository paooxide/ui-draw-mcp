use async_trait::async_trait;
use mcp_types::{CallCtx, Category, Envelope, ErrorCode, Tier, ToolDescriptor, ToolModule};
use serde_json::{json, Value};

use crate::store::{Step, Store, StoreError};

pub struct MemoryModule {
    store: Store,
    max_results: usize,
}

impl MemoryModule {
    pub fn new(store: Store) -> Self {
        MemoryModule {
            store,
            max_results: 10,
        }
    }
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
                 selectors ({role,name,app,window?,index?}), never element refs — refs belong to \
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
                 substring matches ranked by success count. Omit 'goal' to list everything.",
                json!({"type":"object","properties":{
                    "goal":{"type":"string"},
                    "limit":{"type":"integer"}},"required":[]}),
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
                match self.store.find(goal, limit) {
                    Ok(hits) => Envelope::ok(
                        "memory_find",
                        json!({ "recipes": hits, "count": hits.len() }),
                    ),
                    Err(e) => err("memory_find", e),
                }
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
}
