//! The `allowed_apps` rule, in one place so every engine that can name an
//! application applies the same one.

/// Whether `app` is covered by the allowlist: a case-insensitive match on any
/// entry as a substring of the name, which is how `launch` has always read it
/// (`"Code"` covers `"Visual Studio Code"`). An empty list covers nothing.
pub fn app_allowed(allowed: &[String], app: &str) -> bool {
    let lower = app.to_ascii_lowercase();
    allowed
        .iter()
        .any(|a| lower.contains(&a.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::app_allowed;

    #[test]
    fn matches_a_listed_name_as_a_substring_ignoring_case() {
        let allowed = vec!["TextEdit".to_string(), "code".to_string()];
        assert!(app_allowed(&allowed, "textedit"));
        assert!(app_allowed(&allowed, "Visual Studio Code"));
        assert!(!app_allowed(&allowed, "Safari"));
    }

    #[test]
    fn an_empty_list_allows_nothing() {
        assert!(!app_allowed(&[], "Finder"));
    }
}
