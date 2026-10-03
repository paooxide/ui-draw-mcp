//! Configuration loading (`config.toml` + environment).
//!
//! Deliberately a *small, strict* parser for the flat subset the config
//! actually uses (sections, `key = "string"`, `key = 123`, `key = ["a", "b"]`,
//! `#` comments) rather than a full TOML dependency. This matches the
//! project's disk-conscious pattern and keeps the
//! security-relevant config path dependency-free.
//!
//! **Fail-closed:** a config file that exists but does not parse is an error
//! that stops startup. Silently falling back to defaults would be fail-*open*
//! whenever an operator's file was *more* restrictive than the built-in
//! defaults (e.g. `allowed_apps = []`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use mcp_types::Category;

use crate::config::{default_agentctl_dir, Mode, PolicyConfig};

/// One parsed value from the config file.
#[derive(Debug, Clone, PartialEq)]
enum Val {
    Str(String),
    Int(i64),
    Float(f64),
    List(Vec<String>),
}

/// Parse the supported TOML subset into `section.key -> value`.
/// The speeds `demo_speed` accepts. `mcp-input`'s `GlidePreset::from_speed`
/// must accept every one; `agentctl` tests that, because this crate cannot
/// depend on the engine.
pub const DEMO_SPEEDS: [&str; 5] = ["cinematic", "demo", "snappy", "instant", "off"];

/// A demo speed this build knows, normalised to lower case. An unknown name
/// used to fall back to `demo` silently, so `demo_speed = "slow"` ran at the
/// wrong speed and nothing said so.
fn validate_demo_speed(source: &str, value: &str) -> Result<String, String> {
    let v = value.trim().to_ascii_lowercase();
    if DEMO_SPEEDS.contains(&v.as_str()) {
        Ok(v)
    } else {
        Err(format!(
            "unknown {source} '{value}' (expected one of: {})",
            DEMO_SPEEDS.join(", ")
        ))
    }
}

fn parse(text: &str) -> Result<HashMap<String, Val>, String> {
    let mut out = HashMap::new();
    let mut section = String::new();
    for (n, raw) in text.lines().enumerate() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        let lineno = n + 1;
        if let Some(inner) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            let name = inner.trim();
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
            {
                return Err(format!("line {lineno}: bad section header '{line}'"));
            }
            section = name.to_ascii_lowercase();
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            return Err(format!(
                "line {lineno}: expected 'key = value', got '{line}'"
            ));
        };
        let key = k.trim().to_ascii_lowercase();
        if key.is_empty() {
            return Err(format!("line {lineno}: empty key"));
        }
        let val = parse_value(v.trim()).map_err(|e| format!("line {lineno}: {e}"))?;
        let full = if section.is_empty() {
            key
        } else {
            format!("{section}.{key}")
        };
        out.insert(full, val);
    }
    Ok(out)
}

/// Strip a trailing `#` comment, respecting quoted strings.
fn strip_comment(line: &str) -> &str {
    let mut in_str = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_str = !in_str,
            '#' if !in_str => return &line[..i],
            _ => {}
        }
    }
    line
}

/// A boolean setting. TOML spells them `true` and `false`; anything else is an
/// error rather than false, since several settings default to on and a value
/// read as false would silently turn a safeguard off (`human_override = "yes"`
/// used to disable the human-takeover stop).
fn boolean(key: &str, s: &str) -> Result<bool, String> {
    match s {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(format!("{key} must be true or false, not '{other}'")),
    }
}

fn parse_value(s: &str) -> Result<Val, String> {
    if let Some(inner) = s.strip_prefix('[') {
        let inner = inner
            .strip_suffix(']')
            .ok_or_else(|| format!("unterminated array '{s}'"))?;
        let mut items = Vec::new();
        for part in split_top(inner) {
            let p = part.trim();
            if p.is_empty() {
                continue;
            }
            items.push(unquote(p)?);
        }
        return Ok(Val::List(items));
    }
    if s.starts_with('"') {
        return Ok(Val::Str(unquote(s)?));
    }
    if let Ok(i) = s.parse::<i64>() {
        return Ok(Val::Int(i));
    }
    // Floats are accepted only in finite form: `inf`/`nan` would propagate
    // into a comparison threshold and silently disable it.
    if let Ok(f) = s.parse::<f64>() {
        if f.is_finite() {
            return Ok(Val::Float(f));
        }
        return Err(format!("value must be a finite number: '{s}'"));
    }
    if s == "true" || s == "false" {
        return Ok(Val::Str(s.to_string()));
    }
    Err(format!(
        "value must be a quoted string, number, or array: '{s}'"
    ))
}

/// Split on commas that are not inside a quoted string.
fn split_top(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut in_str) = (Vec::new(), String::new(), false);
    for c in s.chars() {
        match c {
            '"' => {
                in_str = !in_str;
                cur.push(c);
            }
            ',' if !in_str => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

fn unquote(s: &str) -> Result<String, String> {
    let t = s.trim();
    let inner = t
        .strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .ok_or_else(|| format!("expected a quoted string, got '{t}'"))?;
    if inner.contains('"') {
        return Err(format!("unescaped quote in string '{t}'"));
    }
    Ok(inner.to_string())
}

/// Where the config lives: `$AGENTCTL_CONFIG`, else `~/.agentctl/config.toml`.
pub fn config_path() -> PathBuf {
    std::env::var_os("AGENTCTL_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| default_agentctl_dir().join("config.toml"))
}

impl PolicyConfig {
    /// Layer a config file's contents onto the secure defaults.
    pub fn from_toml_str(text: &str) -> Result<PolicyConfig, String> {
        let map = parse(text)?;
        let mut cfg = PolicyConfig::default();

        for (key, val) in &map {
            match (key.as_str(), val) {
                ("policy.categories", Val::List(v)) => {
                    let mut cats = Vec::new();
                    for slug in v {
                        cats.push(Category::from_slug(slug).ok_or_else(|| {
                            format!("unknown category '{slug}' in policy.categories")
                        })?);
                    }
                    cfg.categories = cats;
                }
                ("policy.enable", Val::List(v)) => cfg.enable = v.clone(),
                ("policy.anonymize", Val::Str(s)) => cfg.anonymize = boolean(key, s)?,
                ("policy.access", Val::Str(a)) => {
                    cfg.access = Some(crate::Access::parse(a).ok_or_else(|| {
                        format!("unknown policy.access '{a}' (expected ask, auto, or bypass)")
                    })?);
                }
                ("policy.allowed_apps", Val::List(v)) => cfg.allowed_apps = v.clone(),
                ("browser.allowed_origins", Val::List(v)) => cfg.allowed_origins = v.clone(),
                ("browser.allow_private", Val::Str(s)) => cfg.browser_allow_private = s == "true",
                ("judge.enabled", Val::Str(s)) => cfg.judge.enabled = s == "true",
                ("judge.base_url", Val::Str(s)) => cfg.judge.base_url = s.clone(),
                ("judge.model", Val::Str(s)) => cfg.judge.model = s.clone(),
                ("judge.timeout_ms", Val::Int(i)) if *i > 0 => cfg.judge.timeout_ms = *i as u64,
                ("judge.threshold", Val::Float(f)) => cfg.judge.threshold = *f,
                ("judge.threshold", Val::Int(i)) => cfg.judge.threshold = *i as f64,
                ("judge.destructive_threshold", Val::Float(f)) => {
                    cfg.judge.destructive_threshold = Some(*f)
                }
                ("judge.destructive_threshold", Val::Int(i)) => {
                    cfg.judge.destructive_threshold = Some(*i as f64)
                }
                ("judge.injection_threshold", Val::Float(f)) => {
                    cfg.judge.injection_threshold = Some(*f)
                }
                ("judge.injection_threshold", Val::Int(i)) => {
                    cfg.judge.injection_threshold = Some(*i as f64)
                }
                ("judge.match_threshold", Val::Float(f)) => cfg.judge.match_threshold = Some(*f),
                ("judge.match_threshold", Val::Int(i)) => {
                    cfg.judge.match_threshold = Some(*i as f64)
                }
                ("judge.max_state_bytes", Val::Int(i)) if *i > 0 => {
                    cfg.judge.max_state_bytes = *i as usize
                }
                ("judge.enabled" | "judge.base_url" | "judge.model", _) => {
                    return Err(format!("{key} must be a string"))
                }
                ("judge.timeout_ms" | "judge.max_state_bytes", _) => {
                    return Err(format!("{key} must be a positive integer"))
                }
                (
                    "judge.threshold"
                    | "judge.destructive_threshold"
                    | "judge.injection_threshold"
                    | "judge.match_threshold",
                    _,
                ) => return Err(format!("{key} must be a number")),
                ("fs.roots", Val::List(v)) => cfg.fs_roots = v.iter().map(PathBuf::from).collect(),
                ("terminal.allowed_commands", Val::List(v)) => cfg.allowed_commands = v.clone(),
                ("network.allowed_hosts", Val::List(v)) => cfg.allowed_hosts = v.clone(),
                ("credentials.allowed_services", Val::List(v)) => cfg.allowed_services = v.clone(),
                ("input.terminal_apps", Val::List(v)) => cfg.terminal_apps = v.clone(),
                ("terminal.allowed_shells", Val::List(v)) => cfg.allowed_shells = v.clone(),
                ("packages.allowed_sources", Val::List(v)) => cfg.allowed_sources = v.clone(),
                ("packages.allowlist", Val::List(v)) => cfg.package_allowlist = v.clone(),
                ("packages.denylist", Val::List(v)) => cfg.package_denylist = v.clone(),
                ("packages.allow_arbitrary_source", Val::Str(s)) => {
                    cfg.allow_arbitrary_source = s == "true"
                }
                ("terminal.max_pty_sessions", Val::Int(i)) if *i >= 0 => {
                    cfg.max_pty_sessions = *i as usize
                }
                ("terminal.max_pty_buffer", Val::Int(i)) if *i >= 0 => {
                    cfg.max_pty_buffer = *i as usize
                }
                ("vision.detail_low_px", Val::Int(i)) if *i > 0 => {
                    cfg.vision_detail_low_px = *i as u32
                }
                ("vision.detail_balanced_px", Val::Int(i)) if *i > 0 => {
                    cfg.vision_detail_balanced_px = *i as u32
                }
                ("vision.detail_full_px", Val::Int(i)) if *i > 0 => {
                    cfg.vision_detail_full_px = *i as u32
                }
                ("vision.pixels_per_token", Val::Int(i)) if *i > 0 => {
                    cfg.vision_pixels_per_token = *i as u32
                }
                ("vision.max_image_bytes", Val::Int(i)) if *i > 0 => {
                    cfg.vision_max_image_bytes = *i as usize
                }
                ("vision.default_detail", Val::Str(s)) => {
                    if !matches!(s.as_str(), "low" | "balanced" | "medium" | "full" | "high") {
                        return Err(format!(
                            "unknown vision.default_detail '{s}' \
                             (expected low, balanced, or full)"
                        ));
                    }
                    cfg.vision_default_detail = s.clone();
                }
                // An integer is a perfectly reasonable spelling of a threshold,
                // so accept both rather than making the operator write `1.0`.
                ("vision.unchanged_mad", Val::Float(f)) if *f >= 0.0 => {
                    cfg.vision_unchanged_mad = *f
                }
                ("vision.unchanged_mad", Val::Int(i)) if *i >= 0 => {
                    cfg.vision_unchanged_mad = *i as f64
                }
                ("http.enabled", Val::Str(s)) => cfg.http_enabled = s == "true",
                ("http.bind", Val::Str(s)) => cfg.http_bind = s.clone(),
                ("http.token", Val::Str(s)) => cfg.http_token = s.clone(),
                ("http.allowed_origins", Val::List(v)) => cfg.http_allowed_origins = v.clone(),
                ("memory.store", Val::Str(s)) => cfg.memory_store = PathBuf::from(s),
                ("memory.max_recipes", Val::Int(i)) if *i >= 0 => cfg.max_recipes = *i as usize,
                ("input.human_override", Val::Str(s)) => cfg.human_override = s == "true",
                ("input.human_override_px", Val::Int(i)) if *i > 0 => {
                    cfg.human_override_px = *i as u32
                }
                ("input.human_override_grace_ms", Val::Int(i)) if *i >= 0 => {
                    cfg.human_override_grace_ms = *i as u64
                }
                ("terminal.allow_shell", Val::Str(s)) => cfg.allow_shell = s == "true",
                ("network.allow_private", Val::Str(s)) => cfg.allow_private_network = s == "true",
                ("policy.mode", Val::Str(s)) => {
                    cfg.mode =
                        Mode::parse(s).ok_or_else(|| format!("unknown policy.mode '{s}'"))?;
                }
                ("policy.max_denials", Val::Int(i)) if *i >= 0 => cfg.max_denials = *i as usize,
                ("policy.max_consent_prompts", Val::Int(i)) if *i >= 0 => {
                    cfg.max_consent_prompts = *i as usize
                }
                ("policy.kill_switch_file", Val::Str(s)) => cfg.kill_switch_file = PathBuf::from(s),
                ("policy.audit_dir", Val::Str(s)) => cfg.audit_dir = PathBuf::from(s),
                ("policy.audit_signing_key", Val::Str(s)) => {
                    cfg.audit_signing_key = Some(PathBuf::from(s))
                }
                ("policy.default_role" | "policy.role", Val::Str(s)) => {
                    cfg.active_role = Some(s.clone());
                }
                ("policy.demo" | "input.demo" | "demo.enabled", Val::Str(s)) => {
                    cfg.demo = boolean(key, s)?;
                }
                ("policy.demo_speed" | "input.demo_speed" | "demo.speed", Val::Str(s)) => {
                    cfg.demo_speed = validate_demo_speed("demo_speed", s)?;
                }
                (
                    "invariants.protected_paths" | "policy.invariants.protected_paths",
                    Val::List(v),
                ) => {
                    cfg.invariants.protected_paths = v.iter().map(PathBuf::from).collect();
                }
                (
                    "invariants.denied_domains" | "policy.invariants.denied_domains",
                    Val::List(v),
                ) => {
                    cfg.invariants.denied_domains = v.clone();
                }
                (k, val) if k.starts_with("roles.") => {
                    let parts: Vec<&str> = k.split('.').collect();
                    if parts.len() == 3 {
                        let role_name = parts[1];
                        let prop = parts[2];
                        let role = cfg
                            .roles
                            .entry(role_name.to_string())
                            .or_insert_with(|| crate::role::RoleProfile::new(role_name));

                        match (prop, val) {
                            ("description", Val::Str(s)) => role.description = s.clone(),
                            ("allowed_categories", Val::List(v)) => {
                                let mut cats = Vec::new();
                                for slug in v {
                                    cats.push(Category::from_slug(slug).ok_or_else(|| {
                                        format!("unknown category '{slug}' in {k}")
                                    })?);
                                }
                                role.allowed_categories = Some(cats);
                            }
                            ("denied_categories", Val::List(v)) => {
                                let mut cats = Vec::new();
                                for slug in v {
                                    cats.push(Category::from_slug(slug).ok_or_else(|| {
                                        format!("unknown category '{slug}' in {k}")
                                    })?);
                                }
                                role.denied_categories = cats;
                            }
                            ("allowed_tiers", Val::List(v)) => {
                                let mut tiers = Vec::new();
                                for t in v {
                                    match t.to_ascii_lowercase().as_str() {
                                        "read" => tiers.push(mcp_types::Tier::Read),
                                        "standard" => tiers.push(mcp_types::Tier::Standard),
                                        "dangerous" => tiers.push(mcp_types::Tier::Dangerous),
                                        other => {
                                            return Err(format!("unknown tier '{other}' in {k}"))
                                        }
                                    }
                                }
                                role.allowed_tiers = Some(tiers);
                            }
                            ("allowed_tools", Val::List(v)) => role.allowed_tools = Some(v.clone()),
                            ("denied_tools", Val::List(v)) => role.denied_tools = v.clone(),
                            (
                                "require_consent_for"
                                | "require_consent_for_tiers"
                                | "require_consent_tiers",
                                Val::List(v),
                            ) => {
                                let mut tiers = Vec::new();
                                let mut tools = Vec::new();
                                for item in v {
                                    match item.to_ascii_lowercase().as_str() {
                                        "read" => tiers.push(mcp_types::Tier::Read),
                                        "standard" => tiers.push(mcp_types::Tier::Standard),
                                        "dangerous" => tiers.push(mcp_types::Tier::Dangerous),
                                        _ => tools.push(item.clone()),
                                    }
                                }
                                if !tiers.is_empty() {
                                    role.require_consent_for_tiers = tiers;
                                }
                                if !tools.is_empty() {
                                    role.require_consent_for_tools = tools;
                                }
                            }
                            (
                                "require_consent_for_tools" | "require_consent_tools",
                                Val::List(v),
                            ) => role.require_consent_for_tools = v.clone(),
                            ("max_denials", Val::Int(i)) if *i >= 0 => {
                                role.max_denials = Some(*i as usize)
                            }
                            // A misspelt or mistyped role setting would leave the
                            // role looser than its author meant.
                            _ => return Err(format!("{k}: unknown role setting or wrong type")),
                        }
                    }
                }
                // Unknown keys are tolerated for forward-compatibility, but a
                // *known* key with the wrong type is a hard error.
                (
                    "policy.categories"
                    | "policy.enable"
                    | "policy.allowed_apps"
                    | "browser.allowed_origins"
                    | "input.terminal_apps"
                    | "terminal.allowed_shells"
                    | "packages.allowed_sources"
                    | "packages.allowlist"
                    | "packages.denylist"
                    | "http.allowed_origins",
                    _,
                ) => return Err(format!("{key} must be an array of strings")),
                (
                    "terminal.max_pty_sessions" | "terminal.max_pty_buffer" | "memory.max_recipes",
                    _,
                ) => return Err(format!("{key} must be a non-negative integer")),
                (
                    "vision.detail_low_px"
                    | "vision.detail_balanced_px"
                    | "vision.detail_full_px"
                    | "vision.pixels_per_token"
                    | "vision.max_image_bytes",
                    _,
                ) => return Err(format!("{key} must be a positive integer")),
                ("vision.unchanged_mad", _) => {
                    return Err(format!("{key} must be a non-negative number"))
                }
                ("vision.default_detail", _) => return Err(format!("{key} must be a string")),
                ("http.enabled" | "http.bind" | "http.token", _) => {
                    return Err(format!("{key} must be a string"))
                }
                ("memory.store", _) => return Err(format!("{key} must be a string")),
                (
                    "policy.mode"
                    | "policy.kill_switch_file"
                    | "policy.audit_dir"
                    | "policy.audit_signing_key",
                    _,
                ) => return Err(format!("{key} must be a string")),
                ("policy.max_denials" | "policy.max_consent_prompts", _) => {
                    return Err(format!("{key} must be a non-negative integer"))
                }
                ("policy.access", _) => {
                    return Err(format!("{key} must be a string (ask, auto, or bypass)"))
                }
                ("policy.anonymize", _) => {
                    return Err(format!("{key} must be a boolean (true or false)"))
                }
                _ => tracing::warn!(key = %key, "ignoring unknown config key"),
            }
        }
        // A permission profile is a shortcut: it turns on every category and
        // every dangerous tool and sets the interaction mode, so the operator
        // need not list them. The granular `categories`/`enable` are then
        // ignored, and `mode` follows the profile.
        if let Some(access) = cfg.access {
            cfg.categories = crate::all_categories();
            cfg.mode = access.mode();
        }
        // A judge that can never work fails here, at load, rather than on
        // the first call: falling back to defaults could silently drop a
        // judgment the operator meant to have.
        cfg.judge.validate()?;
        Ok(cfg)
    }

    /// Load from [`config_path`]. Missing file → secure defaults. Present but
    /// unparseable → `Err` (startup should abort; see the module docs).
    pub fn load() -> Result<PolicyConfig, String> {
        Self::load_from(&config_path())
    }

    pub fn load_from(path: &Path) -> Result<PolicyConfig, String> {
        let mut cfg = match std::fs::read_to_string(path) {
            Ok(text) => {
                Self::from_toml_str(&text).map_err(|e| format!("{}: {e}", path.display()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => PolicyConfig::default(),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };

        if let Ok(v) = std::env::var("AGENTCTL_ANONYMIZE") {
            match v.to_ascii_lowercase().as_str() {
                "0" | "false" | "off" | "no" => cfg.anonymize = false,
                "1" | "true" | "on" | "yes" => cfg.anonymize = true,
                _ => {
                    return Err(format!(
                        "invalid AGENTCTL_ANONYMIZE value '{v}' (expected true/false)"
                    ))
                }
            }
        } else if std::env::var_os("AGENTCTL_NO_ANONYMIZE").is_some()
            || std::env::var_os("AGENTCTL_NO_PII").is_some()
        {
            cfg.anonymize = false;
        }

        if let Ok(v) = std::env::var("AGENTCTL_DEMO") {
            match v.to_ascii_lowercase().as_str() {
                "0" | "false" | "off" | "no" => cfg.demo = false,
                "1" | "true" | "on" | "yes" => cfg.demo = true,
                _ => {
                    return Err(format!(
                        "invalid AGENTCTL_DEMO value '{v}' (expected true/false)"
                    ))
                }
            }
        }
        if let Ok(v) = std::env::var("AGENTCTL_DEMO_SPEED") {
            cfg.demo_speed = validate_demo_speed("AGENTCTL_DEMO_SPEED", &v)?;
        }

        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sections_arrays_ints_and_comments() {
        let cfg = PolicyConfig::from_toml_str(
            r#"
            # leading comment
            [policy]
            categories = ["vision", "browser"]   # trailing comment
            allowed_apps = ["Safari", "Notes"]
            mode = "autonomous"
            max_denials = 9

            [browser]
            allowed_origins = ["https://ok.example"]
            "#,
        )
        .expect("parses");
        assert_eq!(cfg.categories, vec![Category::Vision, Category::Browser]);
        assert_eq!(cfg.allowed_apps, vec!["Safari", "Notes"]);
        assert_eq!(cfg.mode, Mode::Autonomous);
        assert_eq!(cfg.max_denials, 9);
        assert_eq!(cfg.allowed_origins, vec!["https://ok.example"]);
    }

    #[test]
    fn an_access_profile_enables_everything_and_sets_the_mode() {
        let cfg = PolicyConfig::from_toml_str("[policy]\naccess = \"bypass\"\n").unwrap();
        assert_eq!(cfg.access, Some(crate::Access::Bypass));
        assert_eq!(cfg.categories.len(), crate::all_categories().len());
        assert_eq!(cfg.mode, Mode::Autonomous);
        let cfg = PolicyConfig::from_toml_str("[policy]\naccess = \"ask\"\n").unwrap();
        assert_eq!(cfg.access, Some(crate::Access::Ask));
        assert_eq!(cfg.mode, Mode::Interactive);
        assert!(cfg.categories.contains(&mcp_types::Category::Terminal));
        // A profile overrides a hand-written category list.
        let cfg =
            PolicyConfig::from_toml_str("[policy]\ncategories = [\"vision\"]\naccess = \"auto\"\n")
                .unwrap();
        assert_eq!(cfg.categories.len(), crate::all_categories().len());
        // An unknown profile is a hard error, not a silent default.
        assert!(PolicyConfig::from_toml_str("[policy]\naccess = \"yolo\"\n").is_err());
        assert!(PolicyConfig::from_toml_str("[policy]\naccess = 3\n").is_err());
    }

    #[test]
    fn empty_allowlist_is_honoured_not_replaced_by_defaults() {
        // The fail-open case this design exists to prevent.
        let cfg = PolicyConfig::from_toml_str("[policy]\nallowed_apps = []\n").unwrap();
        assert!(cfg.allowed_apps.is_empty());
    }

    #[test]
    fn hash_inside_a_string_is_not_a_comment() {
        let cfg = PolicyConfig::from_toml_str("[browser]\nallowed_origins = [\"https://a/#/x\"]\n")
            .unwrap();
        assert_eq!(cfg.allowed_origins, vec!["https://a/#/x"]);
    }

    #[test]
    fn malformed_input_is_rejected() {
        for bad in [
            "[policy]\ncategories = vision\n",       // unquoted
            "[policy]\ncategories = [\"nope\"]\n",   // unknown category
            "[policy]\nmode = \"sideways\"\n",       // unknown mode
            "[policy]\nmax_denials = \"five\"\n",    // wrong type
            "[policy]\nallowed_apps = \"Safari\"\n", // scalar where list required
            "not a key value line\n",
            "[policy\ncategories = []\n", // bad header
        ] {
            assert!(
                PolicyConfig::from_toml_str(bad).is_err(),
                "should have rejected: {bad:?}"
            );
        }
    }

    /// The capture tunables round-trip, including the float threshold that
    /// motivated teaching the parser about non-integers.
    #[test]
    fn vision_section_is_parsed() {
        let cfg = PolicyConfig::from_toml_str(
            r#"
            [vision]
            detail_low_px = 640
            detail_balanced_px = 960
            detail_full_px = 1400
            default_detail = "balanced"
            unchanged_mad = 2.5
            pixels_per_token = 800
            max_image_bytes = 4000000
            "#,
        )
        .expect("parses");
        assert_eq!(cfg.vision_detail_low_px, 640);
        assert_eq!(cfg.vision_detail_balanced_px, 960);
        assert_eq!(cfg.vision_detail_full_px, 1400);
        assert_eq!(cfg.vision_default_detail, "balanced");
        assert_eq!(cfg.vision_unchanged_mad, 2.5);
        assert_eq!(cfg.vision_pixels_per_token, 800);
        assert_eq!(cfg.vision_max_image_bytes, 4_000_000);
    }

    /// An integer is a reasonable way to write a threshold; requiring `1.0`
    /// would be a papercut with no upside.
    #[test]
    fn a_float_field_also_accepts_an_integer() {
        let cfg = PolicyConfig::from_toml_str("[vision]\nunchanged_mad = 3\n").unwrap();
        assert_eq!(cfg.vision_unchanged_mad, 3.0);
    }

    /// Defaults must reproduce the previously compiled-in constants, or moving
    /// them into config would be a silent behaviour change for every operator
    /// who has no `[vision]` section.
    #[test]
    fn vision_defaults_match_the_former_constants() {
        let cfg = PolicyConfig::default();
        assert_eq!(cfg.vision_detail_low_px, 768);
        assert_eq!(cfg.vision_detail_balanced_px, 1024);
        assert_eq!(cfg.vision_detail_full_px, 1568);
        assert_eq!(cfg.vision_default_detail, "full");
        assert_eq!(cfg.vision_unchanged_mad, 1.0);
        assert_eq!(cfg.vision_pixels_per_token, 750);
        assert_eq!(cfg.vision_max_image_bytes, 8_000_000);
    }

    /// A threshold of `inf` or `nan` would disable the comparison it controls
    /// while looking like a configured value, so both are refused outright.
    #[test]
    fn non_finite_and_malformed_vision_values_are_rejected() {
        for bad in [
            "[vision]\nunchanged_mad = nan\n",
            "[vision]\nunchanged_mad = inf\n",
            "[vision]\nunchanged_mad = -1.0\n",
            "[vision]\nunchanged_mad = \"lots\"\n",
            "[vision]\ndetail_full_px = \"big\"\n",
            "[vision]\ndetail_full_px = 0\n",
            "[vision]\npixels_per_token = 0\n",
            "[vision]\ndefault_detail = \"enormous\"\n",
            "[vision]\ndefault_detail = 3\n",
        ] {
            assert!(
                PolicyConfig::from_toml_str(bad).is_err(),
                "should have rejected: {bad:?}"
            );
        }
    }

    /// The per-use judge thresholds parse (float or integer) and stay unset
    /// when the operator omits them, so an existing config is unchanged.
    #[test]
    fn per_use_judge_thresholds_parse_and_default_to_unset() {
        let cfg = PolicyConfig::from_toml_str(
            r#"
            [judge]
            threshold = 0.7
            destructive_threshold = 0.5
            injection_threshold = 1
            "#,
        )
        .expect("parses");
        assert_eq!(cfg.judge.destructive_threshold, Some(0.5));
        assert_eq!(cfg.judge.injection_threshold, Some(1.0));
        // Left out, so it inherits the general threshold at use.
        assert_eq!(cfg.judge.match_threshold, None);
        assert_eq!(cfg.judge.match_threshold(), 0.7);
    }

    /// A per-use threshold outside 0..=1 is refused at load, just like the
    /// general one, rather than silently disabling the comparison.
    #[test]
    fn an_out_of_range_per_use_judge_threshold_is_rejected() {
        for bad in [
            "[judge]\nenabled = \"true\"\nmatch_threshold = 1.5\n",
            "[judge]\nenabled = \"true\"\ndestructive_threshold = -0.2\n",
            "[judge]\nenabled = \"true\"\ninjection_threshold = nan\n",
            "[judge]\nenabled = \"true\"\nmatch_threshold = \"high\"\n",
        ] {
            assert!(
                PolicyConfig::from_toml_str(bad).is_err(),
                "should have rejected: {bad:?}"
            );
        }
    }

    #[test]
    fn unknown_keys_are_tolerated() {
        let cfg = PolicyConfig::from_toml_str("[policy]\nfuture_option = \"x\"\n").unwrap();
        assert_eq!(cfg.categories, PolicyConfig::default().categories);
    }

    #[test]
    fn missing_file_yields_defaults() {
        let cfg = PolicyConfig::load_from(Path::new("/nonexistent/agentctl/config.toml")).unwrap();
        assert_eq!(cfg.allowed_apps, PolicyConfig::default().allowed_apps);
    }

    #[test]
    fn parses_demo_settings_and_presets() {
        let toml = r#"
        [policy]
        demo = "true"
        demo_speed = "cinematic"
        "#;
        let cfg = PolicyConfig::from_toml_str(toml).expect("parses");
        assert!(cfg.demo);
        assert_eq!(cfg.demo_speed, "cinematic");

        let toml_input = r#"
        [input]
        demo = "true"
        demo_speed = "snappy"
        "#;
        let cfg_input = PolicyConfig::from_toml_str(toml_input).expect("parses");
        assert!(cfg_input.demo);
        assert_eq!(cfg_input.demo_speed, "snappy");
    }

    #[test]
    fn an_unknown_demo_speed_is_a_config_error_under_every_spelling() {
        for key in [
            "[policy]\ndemo_speed",
            "[input]\ndemo_speed",
            "[demo]\nspeed",
        ] {
            let bad = format!("{key} = \"slow\"\n");
            let err = PolicyConfig::from_toml_str(&bad).unwrap_err();
            assert!(err.contains("slow") && err.contains("cinematic"), "{err}");
        }
        for ok in DEMO_SPEEDS {
            let cfg =
                PolicyConfig::from_toml_str(&format!("[policy]\ndemo_speed = \"{ok}\"\n")).unwrap();
            assert_eq!(cfg.demo_speed, ok);
        }
        // Case and padding are forgiven and normalised.
        let cfg = PolicyConfig::from_toml_str("[policy]\ndemo_speed = \" Snappy \"\n").unwrap();
        assert_eq!(cfg.demo_speed, "snappy");
    }

    #[test]
    fn the_demo_speed_environment_variable_is_validated_too() {
        assert_eq!(
            validate_demo_speed("AGENTCTL_DEMO_SPEED", "OFF").as_deref(),
            Ok("off")
        );
        let err = validate_demo_speed("AGENTCTL_DEMO_SPEED", "turbo").unwrap_err();
        assert!(
            err.contains("AGENTCTL_DEMO_SPEED") && err.contains("turbo"),
            "{err}"
        );
    }
}
