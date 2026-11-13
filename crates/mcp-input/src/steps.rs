//! `input_steps`: up to 20 desktop input actions in one call.
//!
//! Each tool call is a model turn, and an agent filling a dialog spends one on
//! every click and keystroke. A batch removes the turns that carry no decision.
//!
//! The hard part is not running the steps, it is not giving anything up by
//! running them together. `Server::dispatch_call` is the only place that
//! applies the policy tier, the role's allow and deny lists, consent, the
//! hard invariants, the kill switch and the audit record,
//! and it does so for the tool *named in the call*. A batch tool that ran its
//! inner steps by calling the module directly would be checked once, as the
//! batch, and a role that denies `keyboard_type` but not `input_steps` would
//! type through it. So the module never runs a step itself: it hands each one
//! back to the server through a [`StepDispatch`], which is the same gate a
//! lone call goes through, and each step is audited as its own call.
//!
//! This module holds the parts that need no server: reading and validating the
//! batch up front, and the handle the composition root fills in.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mcp_types::{Envelope, Tier};
use serde_json::Value;

/// Most steps in one batch. The same limit as `browser_act` `steps`.
pub const MAX_STEPS: usize = 20;

/// Wall time a batch may spend. Checked before each step, and a step that is
/// still running when it passes is cancelled the way a client cancel would.
/// Generous for twenty ordinary actions, short enough that a wedged app cannot
/// hold the session for minutes.
pub const MAX_BATCH_MS: u64 = 30_000;

/// The tools a batch may run: the ones that act. Reads (`get_selection`,
/// `clipboard_read`) return text the next call should be looking at rather than
/// burying in a list, `input_showcase` only configures pointer gliding, and
/// `input_steps` itself would make a batch of batches whose limits multiply.
pub const BATCHABLE: &[&str] = &[
    "ui_action",
    "set_value",
    "select_text",
    "ui_fill_form",
    "keyboard_type",
    "keyboard_shortcut",
    "mouse_action",
    "scroll",
    "hover",
    "drag_drop",
    "clipboard_write",
];

/// One validated step.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub tool: String,
    pub args: Value,
}

fn rank(t: Tier) -> u8 {
    match t {
        Tier::Read => 0,
        Tier::Standard => 1,
        Tier::Dangerous => 2,
    }
}

/// Read and validate a batch before anything runs.
///
/// `tier_of` says which tier a tool of this module has (`None`: not one of
/// them), and `batch_tier` is the tier the batch itself was admitted at. A step
/// is refused if its tool is not in [`BATCHABLE`], is not this module's, or
/// sits above the batch's tier, so the gate that admitted the batch never
/// admitted a more dangerous call than it was shown. Every error names the
/// step, and the first one found ends the check.
pub fn parse_steps(
    args: &Value,
    tier_of: &dyn Fn(&str) -> Option<Tier>,
    batch_tier: Tier,
) -> Result<Vec<Step>, String> {
    let Some(obj) = args.as_object() else {
        return Err("arguments must be an object with 'steps'".into());
    };
    if let Some(k) = obj.keys().find(|k| k.as_str() != "steps") {
        return Err(format!(
            "unexpected '{k}': a batch takes only 'steps', and each step's arguments go in its own 'args'"
        ));
    }
    let Some(steps) = obj.get("steps").and_then(Value::as_array) else {
        return Err("'steps' must be an array of {tool, args}".into());
    };
    if steps.is_empty() || steps.len() > MAX_STEPS {
        return Err(format!(
            "'steps' needs 1 to {MAX_STEPS} steps, got {}",
            steps.len()
        ));
    }
    let mut out = Vec::with_capacity(steps.len());
    for (i, step) in steps.iter().enumerate() {
        let Some(m) = step.as_object() else {
            return Err(format!("step {i} must be an object {{tool, args}}"));
        };
        if let Some(k) = m.keys().find(|k| !matches!(k.as_str(), "tool" | "args")) {
            return Err(format!(
                "step {i}: unexpected '{k}'; a step is {{tool, args}} and the arguments go inside 'args'"
            ));
        }
        let Some(tool) = m.get("tool").and_then(Value::as_str) else {
            return Err(format!("step {i}: 'tool' must be a tool name"));
        };
        if !BATCHABLE.contains(&tool) {
            return Err(format!(
                "step {i}: '{tool}' cannot run in a batch; the tools that can are {}",
                BATCHABLE.join(", ")
            ));
        }
        match tier_of(tool) {
            None => {
                return Err(format!(
                    "step {i}: '{tool}' is not one of this module's tools"
                ))
            }
            Some(t) if rank(t) > rank(batch_tier) => {
                return Err(format!(
                    "step {i}: '{tool}' is a more dangerous tier than the batch, so it must be called on its own"
                ))
            }
            Some(_) => {}
        }
        let args = match m.get("args") {
            None | Some(Value::Null) => Value::Object(Default::default()),
            Some(a) if a.is_object() => a.clone(),
            Some(_) => return Err(format!("step {i}: 'args' must be an object")),
        };
        out.push(Step {
            tool: tool.to_string(),
            args,
        });
    }
    Ok(out)
}

/// Runs one call through the server's own gate.
///
/// Implemented by the composition root over a server that shares this
/// session's policy, so a step meets the checks a call made on its own would:
/// tier, role, consent, invariants, kill switch and an audit
/// record of its own.
#[async_trait]
pub trait StepDispatch: Send + Sync {
    /// Run `tool` as if the agent had called it, and return what it would have
    /// been sent.
    async fn dispatch(&self, tool: &str, args: Value) -> Envelope;

    /// Ask whatever is running through [`StepDispatch::dispatch`] to stop.
    fn cancel(&self);
}

/// Where the composition root puts the [`StepDispatch`] once the server it
/// reaches exists. Empty until then, and empty again after shutdown, so a
/// batch on a server nobody wired is refused instead of run unchecked.
#[derive(Default)]
pub struct StepRelay(Mutex<Option<Arc<dyn StepDispatch>>>);

impl StepRelay {
    pub fn new() -> Arc<Self> {
        Arc::new(StepRelay::default())
    }

    pub fn bind(&self, dispatch: Arc<dyn StepDispatch>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(dispatch);
    }

    pub fn get(&self) -> Option<Arc<dyn StepDispatch>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Let go of the dispatcher. It holds a server that holds this module, and
    /// letting go ends the loop.
    pub fn release(&self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn standard(tool: &str) -> Option<Tier> {
        BATCHABLE.contains(&tool).then_some(Tier::Standard)
    }

    fn parse(v: Value) -> Result<Vec<Step>, String> {
        parse_steps(&v, &standard, Tier::Standard)
    }

    #[test]
    fn a_batch_is_read_in_order_and_args_default_to_empty() {
        let steps = parse(json!({"steps": [
            {"tool": "ui_action", "args": {"ref": "@e1", "action": "click"}},
            {"tool": "keyboard_shortcut", "args": null},
            {"tool": "scroll"}
        ]}))
        .unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].tool, "ui_action");
        assert_eq!(steps[0].args["ref"], "@e1");
        assert_eq!(steps[1].args, json!({}));
        assert_eq!(steps[2].args, json!({}));
    }

    #[test]
    fn the_size_of_a_batch_is_bounded() {
        assert!(parse(json!({"steps": []})).unwrap_err().contains("1 to 20"));
        let one = json!({"tool": "scroll"});
        assert!(parse(json!({"steps": vec![one.clone(); 20]})).is_ok());
        assert!(parse(json!({"steps": vec![one; 21]}))
            .unwrap_err()
            .contains("got 21"));
        assert!(parse(json!({})).unwrap_err().contains("array"));
        assert!(parse(json!({"steps": "click"}))
            .unwrap_err()
            .contains("array"));
    }

    #[test]
    fn a_tool_that_is_not_batchable_is_refused_by_step_number() {
        for tool in [
            "exec",
            "browser_eval",
            "input_steps",
            "get_selection",
            "clipboard_read",
            "input_showcase",
            "",
        ] {
            let e = parse(json!({"steps": [
                {"tool": "scroll"}, {"tool": tool, "args": {}}
            ]}))
            .unwrap_err();
            assert!(e.starts_with("step 1:"), "{tool}: {e}");
            assert!(e.contains("cannot run in a batch"), "{tool}: {e}");
        }
    }

    /// The gate admitted the batch at one tier. A step above it would be a
    /// call the gate never saw, so it is refused even if it were batchable.
    #[test]
    fn a_step_above_the_tier_of_the_batch_is_refused() {
        let tier = |t: &str| (t == "keyboard_type").then_some(Tier::Dangerous);
        let e = parse_steps(
            &json!({"steps": [{"tool": "keyboard_type", "args": {"text": "x"}}]}),
            &tier,
            Tier::Standard,
        )
        .unwrap_err();
        assert!(
            e.starts_with("step 0:") && e.contains("more dangerous"),
            "{e}"
        );
        assert!(parse_steps(
            &json!({"steps": [{"tool": "keyboard_type", "args": {"text": "x"}}]}),
            &tier,
            Tier::Dangerous
        )
        .is_ok());
    }

    #[test]
    fn a_batchable_name_that_the_module_does_not_have_is_refused() {
        let e = parse_steps(
            &json!({"steps": [{"tool": "scroll"}]}),
            &|_| None,
            Tier::Standard,
        )
        .unwrap_err();
        assert!(e.contains("not one of this module's tools"), "{e}");
    }

    #[test]
    fn a_malformed_step_names_itself() {
        for (v, want) in [
            (json!({"steps": [5]}), "step 0 must be an object"),
            (json!({"steps": [{"args": {}}]}), "step 0: 'tool'"),
            (json!({"steps": [{"tool": 3}]}), "step 0: 'tool'"),
            (
                json!({"steps": [{"tool": "scroll", "args": []}]}),
                "step 0: 'args' must be an object",
            ),
            (
                json!({"steps": [{"tool": "scroll"}, {"tool": "scroll", "ref": "@e1"}]}),
                "step 1: unexpected 'ref'",
            ),
            (
                json!({"steps": [{"tool": "scroll"}], "ref": "@e1"}),
                "unexpected 'ref'",
            ),
        ] {
            let e = parse(v.clone()).unwrap_err();
            assert!(e.contains(want), "{v}: {e}");
        }
        assert!(parse_steps(&json!([1]), &standard, Tier::Standard).is_err());
    }

    #[test]
    fn the_relay_is_empty_until_bound_and_again_after_release() {
        struct Nop;
        #[async_trait]
        impl StepDispatch for Nop {
            async fn dispatch(&self, tool: &str, _args: Value) -> Envelope {
                Envelope::ok(tool, json!({}))
            }
            fn cancel(&self) {}
        }
        let relay = StepRelay::new();
        assert!(relay.get().is_none());
        relay.bind(Arc::new(Nop));
        assert!(relay.get().is_some());
        relay.release();
        assert!(relay.get().is_none());
    }
}
