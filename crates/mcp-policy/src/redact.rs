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

fn marker(v: &Value) -> String {
    let len = match v {
        Value::String(s) => s.len(),
        other => other.to_string().len(),
    };
    format!("\u{2039}redacted:len={len}\u{203a}")
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
    fn empty_redactor_changes_nothing() {
        let r = Redactor::empty();
        let mut v = json!({"password": "secret"});
        r.redact_value(&mut v);
        assert_eq!(v["password"], "secret");
    }
}
