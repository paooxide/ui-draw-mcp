use serde_json::Value;

/// Central secret redactor. Replaces the values of registered secret keys with a
/// length-preserving marker, recursively, in any JSON value — applied to results
/// **and** audit payloads. The registry is empty for the walking skeleton;
/// secret-bearing engines register their fields as they land (arch §8, LLM06).
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    secret_keys: Vec<String>,
}

impl Redactor {
    pub fn new(secret_keys: Vec<String>) -> Self {
        Redactor { secret_keys }
    }

    pub fn empty() -> Self {
        Redactor::default()
    }

    fn is_secret(&self, key: &str) -> bool {
        self.secret_keys.iter().any(|k| k == key)
    }

    /// Recursively redact secret-keyed values in place.
    pub fn redact_value(&self, v: &mut Value) {
        match v {
            Value::Object(map) => {
                for (k, val) in map.iter_mut() {
                    if self.is_secret(k) {
                        *val = Value::String(marker(val));
                    } else {
                        self.redact_value(val);
                    }
                }
            }
            Value::Array(arr) => {
                for e in arr.iter_mut() {
                    self.redact_value(e);
                }
            }
            _ => {}
        }
    }
}

/// Redact the payload of a call the agent flagged secret, for the audit log.
///
/// A password the operator hands the agent to type must not persist in the
/// append-only audit as plaintext. Redaction elsewhere is by key name, but a
/// typed password lives under an ordinary key (`text`, `data`, `value`), so
/// this keys off the caller's own `secret: true` flag instead: when it is set,
/// the payload fields are replaced with the length marker. The flag itself is
/// kept, so the log still shows that a secret was entered, just not what.
/// Applied only to the logged copy; the real value still reaches the engine.
///
/// Applies at every depth, not only the top level: a `browser_flow` call
/// carries its steps in an array, and a step flagged `secret` holds a password
/// as surely as a top-level `keyboard_type` does.
pub fn redact_flagged_payload(v: &mut Value) {
    match v {
        Value::Object(obj) => {
            if obj.get("secret").and_then(Value::as_bool).unwrap_or(false) {
                for key in [
                    "text",
                    "data",
                    "value",
                    "key",
                    "token",
                    "api_key",
                    "secret_key",
                ] {
                    if let Some(val) = obj.get_mut(key) {
                        if !val.is_null() {
                            *val = Value::String(marker(val));
                        }
                    }
                }
            }
            // `browser_flow run` takes its replay-time secrets as a `secrets`
            // object (name -> value). Its values are secret by construction:
            // the caller put them there for that reason, and no `secret` flag
            // sits beside them to key off.
            if let Some(Value::Object(secrets)) = obj.get_mut("secrets") {
                for val in secrets.values_mut() {
                    *val = Value::String(marker(val));
                }
            }
            for child in obj.values_mut() {
                if child.is_object() || child.is_array() {
                    redact_flagged_payload(child);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact_flagged_payload),
        _ => {}
    }
}

fn marker(v: &Value) -> String {
    let len = match v {
        Value::String(s) => s.len(),
        other => other.to_string().len(),
    };
    format!("\u{2039}redacted:len={len}\u{203a}")
}

#[cfg(test)]
mod nested_secret_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn flagged_payload_is_redacted_inside_flow_steps() {
        let mut v = json!({
            "name": "login",
            "steps": [
                { "op": "act", "action": "type", "query": "#user", "value": "alice" },
                { "op": "act", "action": "type", "query": "#pw", "value": "hunter2", "secret": true }
            ]
        });
        redact_flagged_payload(&mut v);
        assert_eq!(v["steps"][0]["value"], "alice");
        assert_eq!(v["steps"][1]["value"], "\u{2039}redacted:len=7\u{203a}");
        assert_eq!(v["steps"][1]["secret"], true);
    }

    #[test]
    fn flow_run_secrets_object_is_redacted_by_value_and_keeps_its_names() {
        let mut v = json!({
            "action": "run",
            "name": "login",
            "target_id": "T1",
            "secrets": { "pw": "hunter2", "otp": "123456" }
        });
        redact_flagged_payload(&mut v);
        assert_eq!(v["secrets"]["pw"], "\u{2039}redacted:len=7\u{203a}");
        assert_eq!(v["secrets"]["otp"], "\u{2039}redacted:len=6\u{203a}");
        // Only the values: the names say which secrets were supplied.
        assert_eq!(v["name"], "login");
        assert_eq!(v["action"], "run");
        assert!(!v.to_string().contains("hunter2"));
        assert!(!v.to_string().contains("123456"));

        // Any depth, and a non-string value is redacted too.
        let mut v = json!({ "calls": [{ "args": { "secrets": { "n": 4242, "pw": "x" } } }] });
        redact_flagged_payload(&mut v);
        assert!(!v.to_string().contains("4242"));
        assert_eq!(
            v["calls"][0]["args"]["secrets"]["pw"],
            "\u{2039}redacted:len=1\u{203a}"
        );

        // A `secrets` that is not an object (or an unrelated key) is left alone.
        let mut v = json!({ "secrets": "plain", "secret": false, "other": { "secrets_count": 2 } });
        redact_flagged_payload(&mut v);
        assert_eq!(v["secrets"], "plain");
        assert_eq!(v["other"]["secrets_count"], 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redacts_nested_secret_keys() {
        let r = Redactor::new(vec!["password".into(), "cookie".into()]);
        let mut v = json!({
            "user": "alice",
            "password": "hunter2",
            "session": { "cookie": "abc123", "id": 7 },
            "list": [{ "password": "x" }]
        });
        r.redact_value(&mut v);
        assert_eq!(v["user"], "alice");
        assert_eq!(v["password"], "\u{2039}redacted:len=7\u{203a}");
        assert_eq!(v["session"]["cookie"], "\u{2039}redacted:len=6\u{203a}");
        assert_eq!(v["session"]["id"], 7);
        assert_eq!(v["list"][0]["password"], "\u{2039}redacted:len=1\u{203a}");
    }

    #[test]
    fn a_secret_flagged_payload_is_redacted_but_the_flag_survives() {
        let mut v = json!({"text": "hunter2", "secret": true, "ref": "@e9"});
        redact_flagged_payload(&mut v);
        assert_eq!(v["text"], "\u{2039}redacted:len=7\u{203a}");
        assert_eq!(v["secret"], true);
        assert_eq!(v["ref"], "@e9");
        // data and value too.
        let mut v = json!({"data": "pw\n", "value": "s3cret", "secret": true});
        redact_flagged_payload(&mut v);
        assert!(v["data"].as_str().unwrap().contains("redacted"));
        assert!(v["value"].as_str().unwrap().contains("redacted"));
        // Not flagged: untouched.
        let mut v = json!({"text": "hello world", "secret": false});
        redact_flagged_payload(&mut v);
        assert_eq!(v["text"], "hello world");
        let mut v = json!({"text": "hello"});
        redact_flagged_payload(&mut v);
        assert_eq!(v["text"], "hello");
        // Non-object is left alone.
        let mut v = json!("x");
        redact_flagged_payload(&mut v);
        assert_eq!(v, json!("x"));
    }

    #[test]
    fn empty_redactor_changes_nothing() {
        let r = Redactor::empty();
        let mut v = json!({"password": "secret"});
        r.redact_value(&mut v);
        assert_eq!(v["password"], "secret");
    }
}
