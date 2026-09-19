//! Trimming the tool list the reference client declares to the model.
//!
//! The server offers around a hundred tools. A model driving one focused task
//! needs a handful; a shorter list is a cheaper prompt and a smaller surface
//! for the model to pick the wrong tool from. When TypeSafe is available this
//! asks a Noul per tool ("is this plausibly needed for the task?") in one
//! batched request, keeps those that clear the match threshold, and always
//! keeps a core set that any GUI task needs (observe, wait, dialogs).
//!
//! Two properties keep it safe:
//!
//! 1. **It only ever narrows what is *declared*, never what is *allowed*.** The
//!    server still gates every call; a trimmed list cannot grant anything, and
//!    a tool that is dropped but called anyway simply comes back `NOT_FOUND`
//!    from the client's own guard.
//! 2. **It degrades to the full list.** No key, an unreachable service, a
//!    malformed reply, or a tool the judge did not score: the tool stays in.
//!    The worst case is the list we would have sent anyway.

use std::collections::{BTreeMap, BTreeSet};

use mcp_policy::mcp_judge::{Answer, Judge, NoulCriteria, Question};
use serde_json::{json, Value};

/// Tools kept no matter what the judge says: the ones a GUI task cannot make
/// progress or hand back a result without. Keeping them unconditionally means
/// a wrong "not needed" can never strand the agent with no way to observe.
pub const CORE_TOOLS: &[&str] = &[
    "get_ui_tree",
    "find_elements",
    "get_element",
    "screenshot",
    "wait_for",
    "list_windows",
    "list_apps",
    "handle_dialogs",
];

/// The outcome of a pruning pass.
pub struct Pruned {
    /// The tool declarations to send, in the original order.
    pub tools: Vec<Value>,
    pub kept: Vec<String>,
    pub dropped: Vec<String>,
    /// True when the judge actually ran; false when the full list stands
    /// because the judge was unavailable or failed.
    pub judged: bool,
}

fn all(tools: &[Value]) -> Pruned {
    let kept = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_string))
        .collect();
    Pruned {
        tools: tools.to_vec(),
        kept,
        dropped: Vec::new(),
        judged: false,
    }
}

/// The tool's name, if it has one.
fn name_of(t: &Value) -> Option<&str> {
    t.get("name").and_then(Value::as_str)
}

/// Ask the judge which tools this task plausibly needs, and return a narrowed
/// list. Falls back to the full list on any failure.
pub async fn prune(judge: &Judge, task: &str, tools: &[Value]) -> Pruned {
    if !judge.available() {
        return all(tools);
    }
    // One Noul per non-core tool, batched into a single request.
    let mut questions: BTreeMap<String, Question> = BTreeMap::new();
    for t in tools {
        let Some(name) = name_of(t) else { continue };
        if CORE_TOOLS.contains(&name) {
            continue;
        }
        let desc: String = t
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(240)
            .collect();
        questions.insert(
            name.to_string(),
            Question::Noul {
                instructions: format!(
                    "Would an agent plausibly need the tool `{name}` to accomplish the task in \
                     `task`? Tool description: {desc}"
                ),
                criteria: Some(NoulCriteria {
                    yes: "The task might reasonably use this tool".into(),
                    no: "The task would not use this tool".into(),
                }),
            },
        );
    }
    if questions.is_empty() {
        return all(tools);
    }
    let asked: Vec<String> = questions.keys().cloned().collect();
    let state = json!({ "task": judge.fit(task) });
    let answers = match judge.ask(state, questions).await {
        Ok(a) => a,
        // Degrade: the judge already logged why; send everything.
        Err(_) => return all(tools),
    };

    let bar = judge.match_threshold();
    let mut keep: BTreeSet<String> = CORE_TOOLS.iter().map(|s| s.to_string()).collect();
    for name in &asked {
        // A tool the judge scored below the bar is dropped; a tool it did not
        // answer for is kept, erring toward capability over economy.
        let drop = matches!(answers.answers.get(name), Some(Answer::Noul { noul }) if *noul < bar);
        if !drop {
            keep.insert(name.clone());
        }
    }

    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    let out: Vec<Value> = tools
        .iter()
        .filter(|t| match name_of(t) {
            Some(n) if keep.contains(n) => {
                kept.push(n.to_string());
                true
            }
            Some(n) => {
                dropped.push(n.to_string());
                false
            }
            // A nameless entry is left in; the declaration builder drops it.
            None => true,
        })
        .cloned()
        .collect();
    Pruned {
        tools: out,
        kept,
        dropped,
        judged: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use mcp_policy::mcp_judge::{JudgeConfig, Transport};
    use std::sync::Mutex;
    use std::time::Duration;

    fn tool(name: &str) -> Value {
        json!({ "name": name, "description": format!("does {name}"),
                "inputSchema": {"type":"object"} })
    }

    /// The tool list a real server offers, in miniature: a couple of core
    /// tools plus several task-specific ones.
    fn tools() -> Vec<Value> {
        vec![
            tool("get_ui_tree"),  // core
            tool("wait_for"),     // core
            tool("keyboard_type"),
            tool("browser_navigate"),
            tool("exec"),
            tool("volume_set"),
        ]
    }

    struct Reply(Mutex<Option<Result<(u16, String), String>>>, Mutex<Option<Value>>);
    #[async_trait]
    impl Transport for Reply {
        async fn post(
            &self,
            _u: &str,
            _k: &str,
            body: &Value,
            _t: Duration,
        ) -> Result<(u16, String), String> {
            *self.1.lock().unwrap() = Some(body.clone());
            self.0
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| Err("exhausted".into()))
        }
    }

    fn judge(reply: Result<(u16, String), String>) -> (Judge, std::sync::Arc<Reply>) {
        let r = std::sync::Arc::new(Reply(Mutex::new(Some(reply)), Mutex::new(None)));
        let cfg = JudgeConfig {
            enabled: true,
            match_threshold: Some(0.6),
            ..JudgeConfig::default()
        };
        struct W(std::sync::Arc<Reply>);
        #[async_trait]
        impl Transport for W {
            async fn post(
                &self,
                u: &str,
                k: &str,
                b: &Value,
                t: Duration,
            ) -> Result<(u16, String), String> {
                self.0.post(u, k, b, t).await
            }
        }
        (
            Judge::with_transport(cfg, Some("k".into()), Box::new(W(r.clone()))),
            r,
        )
    }

    #[tokio::test]
    async fn keeps_the_relevant_and_core_tools_and_drops_the_rest() {
        // keyboard_type and browser_navigate score high; exec and volume_set low.
        let body = r#"{"answers":{
            "keyboard_type":{"type":"noul","noul":0.9},
            "browser_navigate":{"type":"noul","noul":0.8},
            "exec":{"type":"noul","noul":0.1},
            "volume_set":{"type":"noul","noul":0.05}
        }}"#;
        let (j, sent) = judge(Ok((200, body.into())));
        let p = prune(&j, "open a website and type into it", &tools()).await;
        assert!(p.judged);
        // Core tools survive regardless; the two high scorers are kept.
        for keep in ["get_ui_tree", "wait_for", "keyboard_type", "browser_navigate"] {
            assert!(p.kept.contains(&keep.to_string()), "should keep {keep}");
        }
        assert_eq!(p.dropped, vec!["exec", "volume_set"]);
        // Order is preserved.
        assert_eq!(
            p.tools.iter().filter_map(name_of).collect::<Vec<_>>(),
            vec!["get_ui_tree", "wait_for", "keyboard_type", "browser_navigate"]
        );
        // Core tools are never even asked about (no wasted questions).
        let body = sent.1.lock().unwrap().clone().unwrap();
        let qs = body["questions"].as_object().unwrap();
        assert!(!qs.contains_key("get_ui_tree"));
        assert!(qs.contains_key("exec"));
        assert_eq!(body["state"]["task"], "open a website and type into it");
    }

    #[tokio::test]
    async fn a_tool_the_judge_did_not_score_is_kept() {
        // Reply omits exec and volume_set entirely.
        let body = r#"{"answers":{
            "keyboard_type":{"type":"noul","noul":0.9},
            "browser_navigate":{"type":"noul","noul":0.05}
        }}"#;
        let (j, _) = judge(Ok((200, body.into())));
        let p = prune(&j, "type something", &tools()).await;
        // browser_navigate scored low → dropped; the unscored ones stay.
        assert_eq!(p.dropped, vec!["browser_navigate"]);
        assert!(p.kept.contains(&"exec".to_string()));
        assert!(p.kept.contains(&"volume_set".to_string()));
    }

    #[tokio::test]
    async fn a_judge_failure_keeps_the_whole_list() {
        let (j, _) = judge(Ok((500, "boom".into())));
        let p = prune(&j, "anything", &tools()).await;
        assert!(!p.judged, "a failure must not be reported as a judged prune");
        assert_eq!(p.tools.len(), tools().len());
        assert!(p.dropped.is_empty());
    }

    #[tokio::test]
    async fn a_disabled_judge_keeps_the_whole_list_without_asking() {
        let cfg = JudgeConfig::default(); // enabled = false
        let j = Judge::with_transport(cfg, Some("k".into()), Box::new(Reply(Mutex::new(None), Mutex::new(None))));
        let p = prune(&j, "anything", &tools()).await;
        assert!(!p.judged);
        assert_eq!(p.tools.len(), tools().len());
    }
}
