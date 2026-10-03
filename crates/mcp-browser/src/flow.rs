//! Saved browser UI tests: a named sequence of steps (navigate / act / wait /
//! capture / assert) recorded once and replayed deterministically. Replay only
//! calls the deterministic engine, so a green run never needs a model; a
//! failing step stops the run and names itself, which is where an agent takes
//! over. File-backed JSON, keyed by name.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One saved flow. `steps` are opaque step objects (see the `browser_flow`
/// schema); keeping them as raw JSON means the store never has to change when
/// a step gains a field.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Flow {
    pub name: String,
    pub steps: Vec<Value>,
    pub updated_ms: u128,
}

#[derive(Debug)]
pub enum FlowError {
    Invalid(String),
    Io(String),
}

/// Placeholder an older recorder wrote in place of a secret value. Replay
/// refuses it; a saved flow may still carry one.
pub const SECRET_PLACEHOLDER: &str = "\u{2039}secret\u{203a}";

/// The objects of a step that can carry a secret value: the step itself and,
/// for a `fill_form` step, each entry of its `fields`.
fn secret_carriers(step: &Value) -> Vec<&Value> {
    let mut v = vec![step];
    if let Some(fields) = step.get("fields").and_then(Value::as_array) {
        v.extend(fields.iter());
    }
    v
}

fn is_flagged_secret(o: &Value) -> bool {
    o.get("secret").and_then(Value::as_bool) == Some(true) || o.get("secret_ref").is_some()
}

/// Refuse a flow that would persist a secret. A secret step names its value
/// with `secret_ref` and the value is supplied in memory when the flow runs;
/// a step that is flagged secret yet carries a literal value would put that
/// value into the flow store, a plain JSON file. The recorder's placeholder is
/// not a value and is allowed (replay refuses it). Pure; no store access.
pub fn validate_flow_secrets(steps: &[Value]) -> Result<(), String> {
    for (i, step) in steps.iter().enumerate() {
        for o in secret_carriers(step) {
            if !is_flagged_secret(o) {
                continue;
            }
            if let Some(r) = o.get("secret_ref") {
                match r.as_str() {
                    Some(s) if !s.trim().is_empty() => {}
                    _ => return Err(format!("step {i}: 'secret_ref' must be a non-empty string")),
                }
            }
            let has_literal = match o.get("value") {
                None | Some(Value::Null) => false,
                Some(Value::String(s)) => s != SECRET_PLACEHOLDER,
                Some(_) => true,
            };
            if has_literal {
                return Err(format!(
                    "step {i} is marked secret but holds a literal 'value', which would be stored in the flow file. \
                     Remove 'value', set \"secret_ref\": \"<name>\" on the step, and pass {{\"secrets\": {{\"<name>\": \"...\"}}}} when the flow runs"
                ));
            }
        }
    }
    Ok(())
}

/// Every distinct `secret_ref` the steps need, in first-use order.
pub fn secret_refs(steps: &[Value]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for step in steps {
        for o in secret_carriers(step) {
            if let Some(r) = o.get("secret_ref").and_then(Value::as_str) {
                if !out.iter().any(|x| x == r) {
                    out.push(r.to_string());
                }
            }
        }
    }
    out
}

/// The refs the steps need that `secrets` does not supply. Checked before a
/// run starts, so a missing secret fails the run up front rather than after
/// the steps before it have already changed the page.
pub fn missing_secret_refs(steps: &[Value], secrets: &BTreeMap<String, String>) -> Vec<String> {
    secret_refs(steps)
        .into_iter()
        .filter(|r| !secrets.contains_key(r))
        .collect()
}

/// A copy of `step` with each `secret_ref` replaced by its supplied value (in
/// the step's `value`, and in the `value` of any `fill_form` field), for
/// in-memory use by one step. The stored flow is never touched.
pub fn resolve_step_secrets(
    step: &Value,
    secrets: &BTreeMap<String, String>,
) -> Result<Value, String> {
    let mut out = step.clone();
    let fill = |o: &mut Value| -> Result<(), String> {
        let Some(r) = o
            .get("secret_ref")
            .and_then(Value::as_str)
            .map(String::from)
        else {
            return Ok(());
        };
        let v = secrets.get(&r).ok_or_else(|| missing_msg(&r))?;
        if let Some(m) = o.as_object_mut() {
            m.insert("value".into(), Value::String(v.clone()));
            m.insert("secret".into(), Value::Bool(true));
        }
        Ok(())
    };
    fill(&mut out)?;
    if let Some(fields) = out.get_mut("fields").and_then(Value::as_array_mut) {
        for f in fields {
            fill(f)?;
        }
    }
    Ok(out)
}

/// Error text for an unsupplied secret; names the ref and where it goes.
pub fn missing_msg(secret_ref: &str) -> String {
    format!(
        "secret '{secret_ref}' was not supplied: pass {{\"secrets\": {{\"{secret_ref}\": \"...\"}}}} \
         when running the flow (agentctl test reads it from the AGENTCTL_SECRET_<REF> environment variable)"
    )
}

/// File-backed flow store, keyed by name.
pub struct FlowStore {
    path: PathBuf,
    max_flows: usize,
    max_steps: usize,
}

impl FlowStore {
    pub fn new(path: PathBuf, max_flows: usize, max_steps: usize) -> Self {
        FlowStore {
            path,
            max_flows,
            max_steps,
        }
    }

    fn load(&self) -> Result<BTreeMap<String, Flow>, FlowError> {
        match std::fs::read_to_string(&self.path) {
            Ok(t) => serde_json::from_str(&t)
                .map_err(|e| FlowError::Io(format!("{}: {e}", self.path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(FlowError::Io(format!("{}: {e}", self.path.display()))),
        }
    }

    fn persist(&self, map: &BTreeMap<String, Flow>) -> Result<(), FlowError> {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let json = serde_json::to_string_pretty(map)
            .map_err(|e| FlowError::Io(format!("serialize: {e}")))?;
        std::fs::write(&self.path, json)
            .map_err(|e| FlowError::Io(format!("{}: {e}", self.path.display())))
    }

    pub fn save(&self, name: &str, steps: Vec<Value>, now_ms: u128) -> Result<Flow, FlowError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(FlowError::Invalid("flow name must not be empty".into()));
        }
        if steps.is_empty() {
            return Err(FlowError::Invalid("a flow needs at least one step".into()));
        }
        validate_flow_secrets(&steps).map_err(FlowError::Invalid)?;
        if steps.len() > self.max_steps {
            return Err(FlowError::Invalid(format!(
                "a flow may have at most {} steps",
                self.max_steps
            )));
        }
        let mut map = self.load()?;
        if !map.contains_key(name) && map.len() >= self.max_flows {
            return Err(FlowError::Invalid(format!(
                "at most {} flows may be stored",
                self.max_flows
            )));
        }
        let flow = Flow {
            name: name.to_string(),
            steps,
            updated_ms: now_ms,
        };
        map.insert(name.to_string(), flow.clone());
        self.persist(&map)?;
        Ok(flow)
    }

    pub fn get(&self, name: &str) -> Result<Option<Flow>, FlowError> {
        Ok(self.load()?.remove(name.trim()))
    }

    pub fn list(&self) -> Result<Vec<Flow>, FlowError> {
        let mut v: Vec<Flow> = self.load()?.into_values().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(v)
    }

    pub fn delete(&self, name: &str) -> Result<bool, FlowError> {
        let mut map = self.load()?;
        let removed = map.remove(name.trim()).is_some();
        if removed {
            self.persist(&map)?;
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store(tag: &str) -> FlowStore {
        let mut p = std::env::temp_dir();
        p.push(format!("agentctl-flows-{tag}-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&p);
        FlowStore::new(p, 10, 50)
    }

    #[test]
    fn save_get_list_delete_round_trip() {
        let s = store("rt");
        let steps = vec![
            json!({"op":"navigate","url":"https://x"}),
            json!({"op":"assert","text":"hi"}),
        ];
        let f = s.save("login", steps.clone(), 1).unwrap();
        assert_eq!(f.steps.len(), 2);
        assert_eq!(s.get("login").unwrap().unwrap().steps, steps);
        assert_eq!(s.list().unwrap().len(), 1);
        assert!(s.delete("login").unwrap());
        assert!(s.get("login").unwrap().is_none());
        assert!(!s.delete("login").unwrap(), "second delete is a no-op");
    }

    #[test]
    fn saving_again_replaces_the_steps() {
        let s = store("replace");
        s.save("f", vec![json!({"op":"navigate"})], 1).unwrap();
        s.save("f", vec![json!({"op":"assert"}), json!({"op":"wait"})], 2)
            .unwrap();
        assert_eq!(s.get("f").unwrap().unwrap().steps.len(), 2);
        assert_eq!(s.list().unwrap().len(), 1, "same name is one flow");
    }

    fn secrets(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn a_secret_step_with_a_literal_value_is_refused_but_a_ref_is_fine() {
        let ok = vec![
            json!({"op":"act","action":"type","query":"#u","value":"alice"}),
            json!({"op":"act","action":"type","query":"#pw","secret":true,"secret_ref":"pw"}),
            // The old recorder's placeholder is not a value.
            json!({"op":"act","action":"type","query":"#pw","secret":true,"value": SECRET_PLACEHOLDER}),
        ];
        assert_eq!(validate_flow_secrets(&ok), Ok(()));

        let bad = [
            json!({"op":"act","query":"#pw","secret":true,"value":"hunter2"}),
            // A ref alone implies secret, so a value beside it is refused too.
            json!({"op":"act","query":"#pw","secret_ref":"pw","value":"hunter2"}),
            json!({"op":"fill_form","fields":[{"selector":"#pw","secret":true,"value":"hunter2"}]}),
        ];
        for step in bad {
            let e = validate_flow_secrets(&[json!({"op":"navigate"}), step.clone()])
                .expect_err(&step.to_string());
            assert!(e.contains("step 1"), "{e}");
            assert!(e.contains("secret_ref") && e.contains("secrets"), "{e}");
            assert!(
                !e.contains("hunter2"),
                "the error must not echo the value: {e}"
            );
        }
        assert!(validate_flow_secrets(&[json!({"secret_ref": ""})]).is_err());
        assert!(validate_flow_secrets(&[json!({"secret_ref": 3})]).is_err());
        // Unflagged steps keep their values, whatever they look like.
        assert_eq!(
            validate_flow_secrets(&[json!({"op":"act","value":"hunter2"})]),
            Ok(())
        );
    }

    #[test]
    fn saving_a_flow_with_a_literal_secret_is_refused_and_writes_nothing() {
        let s = store("secret-save");
        let e = s
            .save(
                "login",
                vec![json!({"op":"act","query":"#pw","secret":true,"value":"hunter2"})],
                1,
            )
            .unwrap_err();
        assert!(matches!(e, FlowError::Invalid(_)), "{e:?}");
        assert!(s.get("login").unwrap().is_none());
        assert!(!s.path.exists(), "a refused save must not create the file");
    }

    #[test]
    fn secret_refs_are_collected_resolved_and_missing_ones_named() {
        let steps = vec![
            json!({"op":"act","action":"type","query":"#pw","secret":true,"secret_ref":"pw"}),
            json!({"op":"act","action":"type","query":"#otp","secret_ref":"otp"}),
            json!({"op":"act","action":"type","query":"#pw2","secret_ref":"pw"}),
            json!({"op":"fill_form","fields":[{"selector":"#c","secret_ref":"card"},{"selector":"#n","value":"x"}]}),
        ];
        assert_eq!(secret_refs(&steps), vec!["pw", "otp", "card"]);
        assert_eq!(
            missing_secret_refs(&steps, &secrets(&[("pw", "a")])),
            vec!["otp", "card"]
        );
        assert!(missing_secret_refs(
            &steps,
            &secrets(&[("pw", "a"), ("otp", "b"), ("card", "c")])
        )
        .is_empty());

        let all = secrets(&[("pw", "p-val"), ("otp", "o-val"), ("card", "c-val")]);
        let r = resolve_step_secrets(&steps[0], &all).unwrap();
        assert_eq!(r["value"], "p-val");
        assert_eq!(r["secret"], true);
        // The stored step is untouched.
        assert!(steps[0].get("value").is_none());
        let f = resolve_step_secrets(&steps[3], &all).unwrap();
        assert_eq!(f["fields"][0]["value"], "c-val");
        assert_eq!(f["fields"][0]["secret"], true);
        assert_eq!(f["fields"][1]["value"], "x");
        // A step with no ref passes through.
        let plain = json!({"op":"wait","dom_settled":true});
        assert_eq!(resolve_step_secrets(&plain, &all).unwrap(), plain);

        let e = resolve_step_secrets(&steps[1], &secrets(&[("pw", "a")])).unwrap_err();
        assert!(e.contains("'otp'") && e.contains("AGENTCTL_SECRET_"), "{e}");
    }

    #[test]
    fn empty_name_or_no_steps_is_rejected() {
        let s = store("bad");
        assert!(matches!(
            s.save("", vec![json!({})], 1),
            Err(FlowError::Invalid(_))
        ));
        assert!(matches!(s.save("x", vec![], 1), Err(FlowError::Invalid(_))));
    }

    #[test]
    fn the_step_and_flow_caps_are_enforced() {
        let s = FlowStore::new(
            std::env::temp_dir().join(format!("agentctl-flows-cap-{}.json", std::process::id())),
            2,
            3,
        );
        let _ = std::fs::remove_file(
            std::env::temp_dir().join(format!("agentctl-flows-cap-{}.json", std::process::id())),
        );
        assert!(s.save("too-many-steps", vec![json!({}); 4], 1).is_err());
        s.save("a", vec![json!({})], 1).unwrap();
        s.save("b", vec![json!({})], 1).unwrap();
        assert!(s.save("c", vec![json!({})], 1).is_err(), "flow cap is 2");
        // Overwriting an existing flow is allowed at the cap.
        assert!(s.save("a", vec![json!({})], 2).is_ok());
    }
}
