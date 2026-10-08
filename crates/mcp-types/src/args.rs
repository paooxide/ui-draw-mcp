//! Reading numbers out of tool arguments.
//!
//! Models send `"x": "100"` as often as `"x": 100`, and a strict
//! `as_f64()` reads the first as missing: the call fails with "missing 'x'"
//! for an argument that is plainly there, or worse, silently falls back to a
//! default. These accept both, and refuse what is not a finite number.

use serde_json::Value;

/// A finite number from a JSON number or a numeric string (`"100"`, `" 2.5 "`).
pub fn as_f64(v: &Value) -> Option<f64> {
    let n = match v {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    n.is_finite().then_some(n)
}

/// A whole number. `3`, `"3"` and `3.0` qualify; `3.5` does not, because
/// rounding an amount of scroll or a step count would hide a mistake.
pub fn as_i64(v: &Value) -> Option<i64> {
    if let Value::Number(n) = v {
        if let Some(i) = n.as_i64() {
            return Some(i);
        }
    }
    let f = as_f64(v)?;
    // 2^53: beyond it a float no longer holds every integer exactly.
    (f.fract() == 0.0 && f.abs() <= 9_007_199_254_740_992.0).then_some(f as i64)
}

/// A non-negative whole number.
pub fn as_u64(v: &Value) -> Option<u64> {
    if let Value::Number(n) = v {
        if let Some(u) = n.as_u64() {
            return Some(u);
        }
    }
    as_i64(v).and_then(|i| u64::try_from(i).ok())
}

/// `args[key]` as a finite number, if present and numeric.
pub fn f64_arg(args: &Value, key: &str) -> Option<f64> {
    args.get(key).and_then(as_f64)
}

/// `args[key]` as a whole number, if present and numeric.
pub fn i64_arg(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(as_i64)
}

/// `args[key]` as a non-negative whole number, if present and numeric.
pub fn u64_arg(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(as_u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numbers_and_numeric_strings_both_read() {
        assert_eq!(as_f64(&json!(100)), Some(100.0));
        assert_eq!(as_f64(&json!("100")), Some(100.0));
        assert_eq!(as_f64(&json!(" -2.5 ")), Some(-2.5));
        assert_eq!(as_f64(&json!("1e2")), Some(100.0));
    }

    #[test]
    fn things_that_are_not_finite_numbers_are_refused() {
        for bad in [
            json!("abc"),
            json!(""),
            json!("NaN"),
            json!("inf"),
            json!("-infinity"),
            json!(null),
            json!(true),
            json!([1]),
            json!({}),
        ] {
            assert_eq!(as_f64(&bad), None, "{bad}");
        }
    }

    #[test]
    fn integers_accept_whole_floats_and_reject_fractions() {
        assert_eq!(as_i64(&json!(3)), Some(3));
        assert_eq!(as_i64(&json!("3")), Some(3));
        assert_eq!(as_i64(&json!(3.0)), Some(3));
        assert_eq!(as_i64(&json!("-7")), Some(-7));
        assert_eq!(as_i64(&json!(3.5)), None);
        assert_eq!(as_i64(&json!("3.5")), None);
    }

    #[test]
    fn unsigned_rejects_negatives() {
        assert_eq!(as_u64(&json!("20")), Some(20));
        assert_eq!(as_u64(&json!(20)), Some(20));
        assert_eq!(as_u64(&json!(-1)), None);
        assert_eq!(as_u64(&json!("-1")), None);
        assert_eq!(as_u64(&json!(u64::MAX)), Some(u64::MAX));
    }

    #[test]
    fn keyed_readers_treat_a_missing_key_as_none() {
        let a = json!({ "x": "100", "amount": 3, "steps": "20" });
        assert_eq!(f64_arg(&a, "x"), Some(100.0));
        assert_eq!(i64_arg(&a, "amount"), Some(3));
        assert_eq!(u64_arg(&a, "steps"), Some(20));
        assert_eq!(f64_arg(&a, "y"), None);
    }
}
