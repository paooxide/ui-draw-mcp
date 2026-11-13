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

use crate::config::{
    default_agentctl_dir, Access, AccessSource, ConfigNotes, Mode, PolicyConfig, UnknownKey,
};

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

/// Every `section.key` the loader reads. A test checks it against the match
/// arms below, so it cannot drift.
const KNOWN_KEYS: &[&str] = &[
    "browser.allow_private",
    "browser.allowed_origins",
    "credentials.allowed_services",
    "demo.enabled",
    "demo.speed",
    "fs.roots",
    "http.allowed_origins",
    "http.bind",
    "http.enabled",
    "http.token",
    "input.demo",
    "input.demo_speed",
    "input.foreground",
    "input.human_override",
    "input.human_override_grace_ms",
    "input.human_override_px",
    "input.polite_idle_ms",
    "input.polite_max_wait_ms",
    "input.terminal_apps",
    "invariants.denied_domains",
    "invariants.protected_paths",
    "judge.base_url",
    "judge.destructive_threshold",
    "judge.enabled",
    "judge.injection_threshold",
    "judge.match_threshold",
    "judge.max_state_bytes",
    "judge.model",
    "judge.threshold",
    "judge.timeout_ms",
    "memory.max_recipes",
    "memory.store",
    "network.allow_private",
    "network.allowed_hosts",
    "packages.allow_arbitrary_source",
    "packages.allowed_sources",
    "packages.allowlist",
    "packages.denylist",
    "policy.access",
    "policy.allowed_apps",
    "policy.anonymize",
    "policy.audit_dir",
    "policy.audit_signing_key",
    "policy.categories",
    "policy.default_role",
    "policy.demo",
    "policy.demo_speed",
    "policy.dry_run",
    "policy.enable",
    "policy.invariants.denied_domains",
    "policy.invariants.protected_paths",
    "policy.kill_switch_file",
    "policy.max_consent_prompts",
    "policy.max_denials",
    "policy.mode",
    "policy.role",
    "terminal.allow_shell",
    "terminal.allowed_commands",
    "terminal.allowed_shells",
    "terminal.max_pty_buffer",
    "terminal.max_pty_sessions",
    "vision.default_detail",
    "vision.detail_balanced_px",
    "vision.detail_full_px",
    "vision.detail_low_px",
    "vision.max_image_bytes",
    "vision.pixels_per_token",
    "vision.unchanged_mad",
];

/// Settings that have more than one accepted spelling: `(older spelling,
/// canonical spelling)`. The canonical one is what `config.example.toml` uses.
/// The older ones still load, with a warning, and when both spellings of one
/// setting appear with different values loading fails: a hash map used to pick
/// one of them arbitrarily.
const ALIASES: &[(&str, &str)] = &[
    ("demo.enabled", "policy.demo"),
    ("demo.speed", "policy.demo_speed"),
    ("input.demo", "policy.demo"),
    ("input.demo_speed", "policy.demo_speed"),
    ("policy.default_role", "policy.role"),
    (
        "policy.invariants.denied_domains",
        "invariants.denied_domains",
    ),
    (
        "policy.invariants.protected_paths",
        "invariants.protected_paths",
    ),
];

/// Keys that load but do nothing. `max_denials` was documented as aborting a
/// session after that many denied calls; nothing ever counted against it.
const NO_EFFECT_KEYS: &[&str] = &["policy.max_denials"];

/// The spellings of a role's consent list. `require_consent_for` is the one
/// documented; the first three take tiers and tool names, the last two tool
/// names only.
const ROLE_CONSENT_CANONICAL: &str = "require_consent_for";
const ROLE_CONSENT_MIXED: [&str; 3] = [
    "require_consent_for",
    "require_consent_for_tiers",
    "require_consent_tiers",
];
const ROLE_CONSENT_TOOLS: [&str; 2] = ["require_consent_for_tools", "require_consent_tools"];

/// Every setting the loader reads under one spelling, sorted: the keys a
/// reference for operators must document. Leaves out the older spellings and
/// the keys that have no effect.
pub fn canonical_config_keys() -> Vec<&'static str> {
    KNOWN_KEYS
        .iter()
        .copied()
        .filter(|k| !ALIASES.iter().any(|(old, _)| old == k) && !NO_EFFECT_KEYS.contains(k))
        .collect()
}

/// Levenshtein distance between two strings, by characters.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != *cb);
            cur.push(sub.min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// The setting an unknown key was probably meant to be. A setting under the
/// wrong section (`policy.foreground`) is the commonest slip, so a name that is
/// unique across sections wins; otherwise the nearest spelling within a few
/// edits. `None` when nothing is close, so a genuinely new key gets no guess.
pub fn nearest_known_key(key: &str) -> Option<&'static str> {
    let keys = canonical_config_keys();
    let leaf = |k: &str| k.rsplit('.').next().unwrap_or(k).to_string();
    let wanted = leaf(key);
    let same_leaf: Vec<&'static str> = keys.iter().copied().filter(|k| leaf(k) == wanted).collect();
    if let [only] = same_leaf[..] {
        return Some(only);
    }
    let limit = 3.min(key.chars().count() / 3);
    keys.into_iter()
        .map(|k| (edit_distance(key, k), k))
        .filter(|(d, _)| *d <= limit)
        .min_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(b.1)))
        .map(|(_, k)| k)
}

/// Record something the operator should see: logged now, and kept for
/// `agentctl doctor`.
fn note(notes: &mut ConfigNotes, message: String) {
    tracing::warn!("{message}");
    notes.warnings.push(message);
}

fn show(v: &Val) -> String {
    match v {
        Val::Str(s) => format!("\"{s}\""),
        Val::Int(i) => i.to_string(),
        Val::Float(f) => f.to_string(),
        Val::List(l) => format!(
            "[{}]",
            l.iter()
                .map(|x| format!("\"{x}\""))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Two values mean the same setting: lists compare without regard to order.
fn same_value(a: &Val, b: &Val) -> bool {
    match (a, b) {
        (Val::List(x), Val::List(y)) => {
            let (mut x, mut y) = (x.clone(), y.clone());
            x.sort();
            y.sort();
            x == y
        }
        _ => a == b,
    }
}

/// Split a role's consent list into the tiers and the tool names it holds.
fn split_consent(items: &[String]) -> (Vec<mcp_types::Tier>, Vec<String>) {
    let (mut tiers, mut tools) = (Vec::new(), Vec::new());
    for item in items {
        match item.to_ascii_lowercase().as_str() {
            "read" => tiers.push(mcp_types::Tier::Read),
            "standard" => tiers.push(mcp_types::Tier::Standard),
            "dangerous" => tiers.push(mcp_types::Tier::Dangerous),
            _ => tools.push(item.clone()),
        }
    }
    (tiers, tools)
}

/// The config after its spellings are made canonical.
struct Canonical {
    /// `(key, value)` sorted by key, older spellings replaced by the canonical
    /// one, the consent-list spellings removed (see `role_consent`).
    entries: Vec<(String, Val)>,
    /// Per role, the tiers and tool names that need consent.
    role_consent: std::collections::BTreeMap<String, (Vec<mcp_types::Tier>, Vec<String>)>,
}

/// Make every setting appear under one spelling, in sorted key order so that
/// warnings and errors are the same on every run. An older spelling is kept
/// with a warning; two spellings of one setting that disagree are an error.
fn canonicalize(map: HashMap<String, Val>, notes: &mut ConfigNotes) -> Result<Canonical, String> {
    let mut sorted: Vec<(String, Val)> = map.into_iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));

    // Group the spellings of each aliased setting under its canonical key.
    let mut groups: std::collections::BTreeMap<String, Vec<(String, Val)>> =
        std::collections::BTreeMap::new();
    let mut entries = Vec::new();
    // role -> (spelling, value), for the consent list.
    let mut consent: std::collections::BTreeMap<String, Vec<(String, Val)>> =
        std::collections::BTreeMap::new();
    for (key, val) in sorted {
        let canonical = ALIASES
            .iter()
            .find(|(old, _)| *old == key)
            .map(|(_, c)| *c)
            .unwrap_or(key.as_str());
        let parts: Vec<&str> = key.split('.').collect();
        if parts.len() == 3
            && parts[0] == "roles"
            && (ROLE_CONSENT_MIXED.contains(&parts[2]) || ROLE_CONSENT_TOOLS.contains(&parts[2]))
        {
            consent
                .entry(parts[1].to_string())
                .or_default()
                .push((key.clone(), val));
        } else if canonical != key || ALIASES.iter().any(|(_, c)| *c == key) {
            groups
                .entry(canonical.to_string())
                .or_default()
                .push((key.clone(), val));
        } else {
            entries.push((key, val));
        }
    }

    for (canonical, spellings) in groups {
        if let Some((first_key, first)) = spellings.first() {
            for (other_key, other) in &spellings[1..] {
                if !same_value(first, other) {
                    return Err(format!(
                        "{first_key} = {} and {other_key} = {} are two spellings of one setting \
                         and disagree; keep only {canonical}",
                        show(first),
                        show(other)
                    ));
                }
            }
        }
        for (spelling, _) in spellings.iter().filter(|(k, _)| *k != canonical) {
            note(
                notes,
                format!("{spelling} is an older spelling of {canonical}; use {canonical}"),
            );
        }
        let value = spellings
            .into_iter()
            .next()
            .map(|(_, v)| v)
            .expect("a group has at least one spelling");
        entries.push((canonical, value));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut role_consent = std::collections::BTreeMap::new();
    for (role, spellings) in consent {
        let mut tiers: Option<Vec<mcp_types::Tier>> = None;
        let mut tools: Option<(String, Vec<String>)> = None;
        for (key, val) in &spellings {
            let Val::List(items) = val else {
                return Err(format!("{key}: unknown role setting or wrong type"));
            };
            let prop = key.rsplit('.').next().unwrap_or_default();
            let (t, n) = if ROLE_CONSENT_MIXED.contains(&prop) {
                split_consent(items)
            } else {
                (Vec::new(), items.clone())
            };
            if !t.is_empty() {
                match &tiers {
                    Some(prev) if !same_set(prev, &t) => {
                        return Err(format!(
                            "roles.{role}: {key} and another spelling of its consent list \
                             name different tiers; keep only roles.{role}.{ROLE_CONSENT_CANONICAL}"
                        ));
                    }
                    _ => tiers = Some(t),
                }
            }
            if !n.is_empty() {
                match &tools {
                    Some((other, prev)) if !same_set(prev, &n) => {
                        return Err(format!(
                            "roles.{role}: {other} and {key} name different tools; keep only \
                             roles.{role}.{ROLE_CONSENT_CANONICAL}"
                        ));
                    }
                    _ => tools = Some((key.clone(), n)),
                }
            }
            if prop != ROLE_CONSENT_CANONICAL {
                note(
                    notes,
                    format!(
                        "{key} is an older spelling of roles.{role}.{ROLE_CONSENT_CANONICAL}; \
                         use {ROLE_CONSENT_CANONICAL} (it takes tiers and tool names)"
                    ),
                );
            }
        }
        role_consent.insert(
            role,
            (
                tiers.unwrap_or_default(),
                tools.map(|(_, t)| t).unwrap_or_default(),
            ),
        );
    }
    Ok(Canonical {
        entries,
        role_consent,
    })
}

/// Two lists hold the same elements, in any order.
fn same_set<T: PartialEq>(a: &[T], b: &[T]) -> bool {
    a.iter().all(|x| b.contains(x)) && b.iter().all(|x| a.contains(x))
}

/// The effective `access` once a flag or `AGENTCTL_ACCESS` is laid over the
/// config file's, and where it came from.
///
/// Like `tighten_foreground`, a pure function over every pair, with one rule:
/// the levels run `ask` < `auto` < `bypass` in how much they let through, and
/// when the file sets `access` the flag or environment may only move it toward
/// `ask`. A request that would loosen the file is an error, not a silent
/// ignore. When the file sets nothing the request sets the level (whoever
/// starts the process is the authority), except that `bypass` is never taken
/// from the environment on its own: an inherited variable is too easy to leave
/// set by accident, so it needs the file or the explicit `--access bypass`.
///
/// `requested` is `(level, Flag)` or `(level, Env)`.
pub fn resolve_access(
    file: Option<Access>,
    requested: Option<(Access, AccessSource)>,
) -> Result<Option<(Access, AccessSource)>, String> {
    let Some((want, via)) = requested else {
        return Ok(file.map(|a| (a, AccessSource::File)));
    };
    let setter = match via {
        AccessSource::Env => "AGENTCTL_ACCESS",
        _ => "--access",
    };
    match file {
        Some(have) if want.permissiveness() > have.permissiveness() => Err(format!(
            "{setter}={} would loosen the config file's access = \"{}\": a flag or environment \
             variable may only move access toward ask (ask < auto < bypass). Edit access in the \
             config file if the looser level is intended",
            want.as_str(),
            have.as_str()
        )),
        Some(have) if have == want => Ok(Some((have, AccessSource::File))),
        Some(_) => Ok(Some((want, via))),
        None if want == Access::Bypass && via == AccessSource::Env => Err(
            "AGENTCTL_ACCESS=bypass is not honoured on its own: an environment variable is too \
             easy to inherit by accident. Set access = \"bypass\" in the config file, or pass \
             --access bypass"
                .to_string(),
        ),
        None => Ok(Some((want, via))),
    }
}

/// `AGENTCTL_ACCESS`, validated. An empty value counts as unset.
fn access_from_env() -> Result<Option<Access>, String> {
    match std::env::var("AGENTCTL_ACCESS") {
        Ok(v) if !v.trim().is_empty() => Access::parse(v.trim()).map(Some).ok_or_else(|| {
            format!("invalid AGENTCTL_ACCESS value '{v}' (expected ask, auto, or bypass)")
        }),
        _ => Ok(None),
    }
}

/// What the file said that `finalize` needs after the keys are read.
#[derive(Default)]
struct FileFacts {
    access: Option<Access>,
    categories_listed: bool,
    enable_listed: bool,
    mode: Option<Mode>,
    dry_run: bool,
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

/// `allow`, `polite` or `never`. Anything else is an error: this setting
/// restricts, so a misspelling read as `allow` would drop the restriction
/// without a word.
fn parse_foreground(source: &str, value: &str) -> Result<mcp_types::Foreground, String> {
    mcp_types::Foreground::parse(value).ok_or_else(|| {
        format!("unknown {source} '{value}' (expected \"allow\", \"polite\" or \"never\")")
    })
}

/// The file's setting after the environment's `AGENTCTL_FOREGROUND`: the
/// stricter of the two wins (`never` over `polite` over `allow`), and an
/// invalid value is an error from either.
fn tighten_foreground(
    from_file: mcp_types::Foreground,
    env: &str,
) -> Result<mcp_types::Foreground, String> {
    let from_env = parse_foreground("AGENTCTL_FOREGROUND", env)?;
    Ok(from_file.tighten(from_env))
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

/// Keep a key nothing reads, with a guess at what was meant. A warning and not
/// an error: a config written for a newer build should still load.
fn record_unknown(notes: &mut ConfigNotes, key: &str) {
    let suggestion = nearest_known_key(key).map(str::to_string);
    match &suggestion {
        Some(s) => tracing::warn!(key = %key, "ignoring unknown config key; did you mean {s}?"),
        None => tracing::warn!(key = %key, "ignoring unknown config key"),
    }
    notes.unknown_keys.push(UnknownKey {
        key: key.to_string(),
        suggestion,
    });
}

impl PolicyConfig {
    /// Layer a config file's contents onto the secure defaults.
    pub fn from_toml_str(text: &str) -> Result<PolicyConfig, String> {
        let (cfg, facts) = Self::parse_config(text)?;
        Self::finalize(cfg, &facts, None, false)
    }

    /// Read the file's keys onto the defaults. `access`, `categories` and the
    /// legacy `enable` and `mode` are only noted here: how they combine is
    /// decided in [`Self::finalize`], once a flag or the environment has had
    /// its say.
    fn parse_config(text: &str) -> Result<(PolicyConfig, FileFacts), String> {
        let mut cfg = PolicyConfig::default();
        let mut facts = FileFacts::default();
        let Canonical {
            entries,
            role_consent,
        } = canonicalize(parse(text)?, &mut cfg.notes)?;

        for (key, val) in &entries {
            match (key.as_str(), val) {
                ("policy.categories", Val::List(v)) => {
                    let mut cats = Vec::new();
                    for slug in v {
                        cats.push(Category::from_slug(slug).ok_or_else(|| {
                            format!("unknown category '{slug}' in policy.categories")
                        })?);
                    }
                    cfg.categories = cats;
                    facts.categories_listed = true;
                }
                ("policy.enable", Val::List(v)) => {
                    cfg.enable = v.clone();
                    facts.enable_listed = true;
                }
                ("policy.anonymize", Val::Str(s)) => cfg.anonymize = boolean(key, s)?,
                ("policy.access", Val::Str(a)) => {
                    facts.access = Some(Access::parse(a).ok_or_else(|| {
                        format!("unknown policy.access '{a}' (expected ask, auto, or bypass)")
                    })?);
                }
                ("policy.dry_run", Val::Str(s)) => facts.dry_run = boolean(key, s)?,
                ("policy.allowed_apps", Val::List(v)) => cfg.allowed_apps = v.clone(),
                ("browser.allowed_origins", Val::List(v)) => cfg.allowed_origins = v.clone(),
                ("browser.allow_private", Val::Str(s)) => {
                    cfg.browser_allow_private = boolean(key, s)?
                }
                ("judge.enabled", Val::Str(s)) => cfg.judge.enabled = boolean(key, s)?,
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
                    cfg.allow_arbitrary_source = boolean(key, s)?
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
                ("http.enabled", Val::Str(s)) => cfg.http_enabled = boolean(key, s)?,
                ("http.bind", Val::Str(s)) => cfg.http_bind = s.clone(),
                ("http.token", Val::Str(s)) => cfg.http_token = s.clone(),
                ("http.allowed_origins", Val::List(v)) => cfg.http_allowed_origins = v.clone(),
                ("memory.store", Val::Str(s)) => cfg.memory_store = PathBuf::from(s),
                ("memory.max_recipes", Val::Int(i)) if *i >= 0 => cfg.max_recipes = *i as usize,
                ("input.foreground", Val::Str(s)) => {
                    cfg.foreground = parse_foreground(key, s)?;
                }
                ("input.polite_idle_ms", Val::Int(i)) if *i >= 0 => cfg.polite_idle_ms = *i as u64,
                ("input.polite_max_wait_ms", Val::Int(i)) if *i > 0 => {
                    cfg.polite_max_wait_ms = *i as u64
                }
                ("input.human_override", Val::Str(s)) => cfg.human_override = boolean(key, s)?,
                ("input.human_override_px", Val::Int(i)) if *i > 0 => {
                    cfg.human_override_px = *i as u32
                }
                ("input.human_override_grace_ms", Val::Int(i)) if *i >= 0 => {
                    cfg.human_override_grace_ms = *i as u64
                }
                ("terminal.allow_shell", Val::Str(s)) => cfg.allow_shell = boolean(key, s)?,
                ("network.allow_private", Val::Str(s)) => {
                    cfg.allow_private_network = boolean(key, s)?
                }
                ("policy.mode", Val::Str(s)) => {
                    cfg.mode =
                        Mode::parse(s).ok_or_else(|| format!("unknown policy.mode '{s}'"))?;
                    facts.mode = Some(cfg.mode);
                }
                // Documented as ending a session after that many denied calls;
                // nothing ever counted against it. Still accepted, so a config
                // that carries it keeps loading.
                ("policy.max_denials", _) => note(
                    &mut cfg.notes,
                    "policy.max_denials has no effect and never had: nothing counted denied \
                     calls against it. Remove it"
                        .to_string(),
                ),
                ("policy.max_consent_prompts", Val::Int(i)) if *i >= 0 => {
                    cfg.max_consent_prompts = *i as usize
                }
                ("policy.kill_switch_file", Val::Str(s)) => cfg.kill_switch_file = PathBuf::from(s),
                ("policy.audit_dir", Val::Str(s)) => cfg.audit_dir = PathBuf::from(s),
                ("policy.audit_signing_key", Val::Str(s)) => {
                    cfg.audit_signing_key = Some(PathBuf::from(s))
                }
                ("policy.role", Val::Str(s)) => {
                    cfg.active_role = Some(s.clone());
                }
                ("policy.demo", Val::Str(s)) => {
                    cfg.demo = boolean(key, s)?;
                }
                ("policy.demo_speed", Val::Str(s)) => {
                    cfg.demo_speed = validate_demo_speed("demo_speed", s)?;
                }
                ("invariants.protected_paths", Val::List(v)) => {
                    cfg.invariants.protected_paths = v.iter().map(PathBuf::from).collect();
                }
                ("invariants.denied_domains", Val::List(v)) => {
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
                            // Documented as a per-role cap on the denial budget;
                            // like the policy-level key it never did anything.
                            ("max_denials", _) => note(
                                &mut cfg.notes,
                                format!(
                                    "{k} has no effect and never had: nothing counted denied \
                                     calls against it. Remove it"
                                ),
                            ),
                            // A misspelt or mistyped role setting would leave the
                            // role looser than its author meant.
                            _ => return Err(format!("{k}: unknown role setting or wrong type")),
                        }
                    } else {
                        // `roles.a.b.c` is no role setting: say so rather than
                        // dropping a role's restriction without a word.
                        record_unknown(&mut cfg.notes, k);
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
                ("policy.max_consent_prompts", _) => {
                    return Err(format!("{key} must be a non-negative integer"))
                }
                ("policy.access", _) => {
                    return Err(format!("{key} must be a string (ask, auto, or bypass)"))
                }
                ("policy.anonymize", _) => {
                    return Err(format!("{key} must be a boolean (true or false)"))
                }
                // A setting this loader reads, given a value of the wrong type,
                // must not be dropped as if it were unknown.
                (k, _) if KNOWN_KEYS.contains(&k) => {
                    return Err(format!("{key} has the wrong type for this setting"))
                }
                _ => record_unknown(&mut cfg.notes, key),
            }
        }
        for (role, (tiers, tools)) in role_consent {
            let profile = cfg
                .roles
                .entry(role.clone())
                .or_insert_with(|| crate::role::RoleProfile::new(role));
            if !tiers.is_empty() {
                profile.require_consent_for_tiers = tiers;
            }
            if !tools.is_empty() {
                profile.require_consent_for_tools = tools;
            }
        }
        // A judge that can never work fails here, at load, rather than on
        // the first call: falling back to defaults could silently drop a
        // judgment the operator meant to have.
        cfg.judge.validate()?;
        Ok((cfg, facts))
    }

    /// Combine what the file said with the flag and environment.
    ///
    /// `requested` is the `--access` flag or `AGENTCTL_ACCESS` (see
    /// [`resolve_access`]); `dry_run` is the `--dry-run` flag.
    fn finalize(
        mut cfg: PolicyConfig,
        facts: &FileFacts,
        requested: Option<(Access, AccessSource)>,
        dry_run: bool,
    ) -> Result<PolicyConfig, String> {
        let resolved = resolve_access(facts.access, requested)?;
        cfg.notes.access_source = resolved.map_or(AccessSource::Default, |(_, source)| source);
        if let Some((access, source)) = resolved {
            cfg.access = Some(access);
            // The level turns every category on, and a `categories` list in the
            // file narrows that: it used to be discarded, so a list written to
            // limit a session did nothing once `access` was added.
            if !facts.categories_listed {
                cfg.categories = crate::all_categories();
            }
            cfg.mode = access.mode();
            let level = format!("access = \"{}\" ({})", access.as_str(), source.describe());
            if facts.enable_listed {
                note(
                    &mut cfg.notes,
                    format!(
                        "policy.enable is ignored because {level} is set: the level already \
                         makes every dangerous tool callable under its own rules. Remove \
                         policy.enable"
                    ),
                );
            }
            // `dry_run` is a way to rehearse, not a choice of level, so it is
            // not a conflict: it is applied below.
            if facts.mode.is_some_and(|m| m != Mode::DryRun) {
                note(
                    &mut cfg.notes,
                    format!(
                        "policy.mode is ignored because {level} is set: the level decides \
                         whether anyone is asked. Remove policy.mode (policy.dry_run = true \
                         is how to rehearse)"
                    ),
                );
            }
        }
        // Dry run works with every level and can only make a session do less.
        // `mode = "dry_run"` is the older spelling and keeps working.
        if dry_run || facts.dry_run || facts.mode == Some(Mode::DryRun) {
            cfg.mode = Mode::DryRun;
        }
        Ok(cfg)
    }

    /// The warning for a role name that still resolves but is no longer
    /// documented, if the active role is one. Roles can be set after loading
    /// (`--role`, `AGENTCTL_ROLE`), so this is asked of the final value.
    pub fn deprecated_role_note(&self) -> Option<String> {
        self.active_role
            .as_deref()
            .and_then(|r| crate::role::deprecated_role_note(r, &self.roles))
    }

    /// Load from [`config_path`]. Missing file → secure defaults. Present but
    /// unparseable → `Err` (startup should abort; see the module docs).
    pub fn load() -> Result<PolicyConfig, String> {
        Self::load_from(&config_path())
    }

    pub fn load_from(path: &Path) -> Result<PolicyConfig, String> {
        Self::load_from_with(path, None, false)
    }

    /// [`Self::load`] with the command line's `--access` and `--dry-run`.
    pub fn load_with(
        access_flag: Option<Access>,
        dry_run_flag: bool,
    ) -> Result<PolicyConfig, String> {
        Self::load_from_with(&config_path(), access_flag, dry_run_flag)
    }

    /// [`Self::load_from`] with the command line's `--access` and `--dry-run`.
    /// A flag stands in for `AGENTCTL_ACCESS` when both are given.
    pub fn load_from_with(
        path: &Path,
        access_flag: Option<Access>,
        dry_run_flag: bool,
    ) -> Result<PolicyConfig, String> {
        let (cfg, facts) = match std::fs::read_to_string(path) {
            Ok(text) => {
                Self::parse_config(&text).map_err(|e| format!("{}: {e}", path.display()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::parse_config("")?,
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let from_env = access_from_env()?;
        let requested = access_flag
            .map(|a| (a, AccessSource::Flag))
            .or(from_env.map(|a| (a, AccessSource::Env)));
        let mut cfg = Self::finalize(cfg, &facts, requested, dry_run_flag)?;

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

        // The environment can add the restriction but never remove it, so a
        // launcher that sets this cannot be undone by a config file edit, and
        // a stray `allow` in the environment cannot undo the file's `never`.
        if let Ok(v) = std::env::var("AGENTCTL_FOREGROUND") {
            cfg.foreground = tighten_foreground(cfg.foreground, &v)?;
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
            max_consent_prompts = 9

            [browser]
            allowed_origins = ["https://ok.example"]
            "#,
        )
        .expect("parses");
        assert_eq!(cfg.categories, vec![Category::Vision, Category::Browser]);
        assert_eq!(cfg.allowed_apps, vec!["Safari", "Notes"]);
        assert_eq!(cfg.mode, Mode::Autonomous);
        assert_eq!(cfg.max_consent_prompts, 9);
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
        // A category list in the file narrows the level (it used to be
        // discarded, which left a list written to limit a session doing nothing).
        let cfg =
            PolicyConfig::from_toml_str("[policy]\ncategories = [\"vision\"]\naccess = \"auto\"\n")
                .unwrap();
        assert_eq!(cfg.categories, vec![Category::Vision]);
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
            "[policy]\ncategories = vision\n",            // unquoted
            "[policy]\ncategories = [\"nope\"]\n",        // unknown category
            "[policy]\nmode = \"sideways\"\n",            // unknown mode
            "[policy]\nmax_consent_prompts = \"five\"\n", // wrong type
            "[policy]\nallowed_apps = \"Safari\"\n",      // scalar where list required
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

    #[test]
    fn known_keys_lists_every_key_the_loader_matches() {
        let src = include_str!("load.rs");
        let start = src.find("for (key, val) in &entries {").unwrap();
        let end = src
            .find("// A setting this loader reads, given a value")
            .unwrap();
        let body = &src[start..end];
        let mut i = 0;
        while let Some(q) = body[i..].find('"') {
            let a = i + q + 1;
            let Some(len) = body[a..].find('"') else {
                break;
            };
            let lit = &body[a..a + len];
            // `"roles."` is a prefix match, not a key.
            let is_key = lit.contains('.')
                && !lit.ends_with('.')
                && lit
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_' || c == '.');
            if is_key {
                assert!(
                    KNOWN_KEYS.contains(&lit),
                    "{lit} is matched but not in KNOWN_KEYS"
                );
            }
            i = a + len + 1;
        }
    }

    #[test]
    fn booleans_are_strict() {
        for bad in ["yes", "True", "1", "on"] {
            let err =
                PolicyConfig::from_toml_str(&format!("[input]\nhuman_override = \"{bad}\"\n"))
                    .unwrap_err();
            assert!(err.contains("must be true or false"), "{err}");
        }
        let cfg = PolicyConfig::from_toml_str("[input]\nhuman_override = false\n").unwrap();
        assert!(!cfg.human_override);
    }

    #[test]
    fn a_known_key_with_the_wrong_type_is_an_error() {
        let err =
            PolicyConfig::from_toml_str("[policy]\nmax_consent_prompts = \"5\"\n").unwrap_err();
        assert!(err.contains("max_consent_prompts"), "{err}");
        // Unknown keys stay tolerated for forward compatibility.
        assert!(PolicyConfig::from_toml_str("[policy]\nsome_future_key = 1\n").is_ok());
    }

    #[test]
    fn foreground_defaults_to_allow_and_never_is_read_from_the_input_table() {
        // The default is the contract that existing users see no change.
        assert_eq!(
            PolicyConfig::default().foreground,
            mcp_types::Foreground::Allow
        );
        assert_eq!(
            PolicyConfig::from_toml_str("").unwrap().foreground,
            mcp_types::Foreground::Allow
        );
        let cfg = PolicyConfig::from_toml_str("[input]\nforeground = \"never\"\n").unwrap();
        assert_eq!(cfg.foreground, mcp_types::Foreground::Never);
        let cfg = PolicyConfig::from_toml_str("[input]\nforeground = \"allow\"\n").unwrap();
        assert_eq!(cfg.foreground, mcp_types::Foreground::Allow);
    }

    #[test]
    fn an_unknown_foreground_value_is_a_config_error_not_allow() {
        for bad in ["Never", "NEVER", "deny", "Polite", "", "yes", "true"] {
            let err = PolicyConfig::from_toml_str(&format!("[input]\nforeground = \"{bad}\"\n"))
                .unwrap_err();
            assert!(
                err.contains("input.foreground")
                    && err.contains("allow")
                    && err.contains("polite")
                    && err.contains("never"),
                "{bad:?}: {err}"
            );
        }
        // Not a string at all: a bare word, a boolean, a number.
        for bad in ["never", "true", "1", "[\"never\"]"] {
            assert!(
                PolicyConfig::from_toml_str(&format!("[input]\nforeground = {bad}\n")).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn polite_is_read_from_the_input_table_with_its_two_timings() {
        let cfg = PolicyConfig::from_toml_str("[input]\nforeground = \"polite\"\n").unwrap();
        assert_eq!(cfg.foreground, mcp_types::Foreground::Polite);
        // Defaults when the keys are absent.
        assert_eq!(
            (cfg.polite_idle_ms, cfg.polite_max_wait_ms),
            (1_500, 15_000)
        );
        let cfg = PolicyConfig::from_toml_str(
            "[input]\nforeground = \"polite\"\npolite_idle_ms = 800\npolite_max_wait_ms = 4000\n",
        )
        .unwrap();
        assert_eq!((cfg.polite_idle_ms, cfg.polite_max_wait_ms), (800, 4_000));
        // Zero idle is allowed (borrow at once); a zero wait would refuse
        // every call, and a negative number is not a duration.
        assert!(PolicyConfig::from_toml_str("[input]\npolite_idle_ms = 0\n").is_ok());
        for bad in [
            "polite_max_wait_ms = 0",
            "polite_idle_ms = -1",
            "polite_idle_ms = \"slow\"",
        ] {
            assert!(
                PolicyConfig::from_toml_str(&format!("[input]\n{bad}\n")).is_err(),
                "{bad}"
            );
        }
    }

    /// The environment can only tighten, over every pair of file and
    /// environment values: `polite` over a file's `allow` applies, and
    /// neither `allow` nor `polite` loosens a stricter file.
    #[test]
    fn the_environment_can_add_restriction_but_not_remove_it() {
        use mcp_types::Foreground::{Allow, Never, Polite};
        for (file, env, want) in [
            (Allow, "allow", Allow),
            (Allow, "polite", Polite),
            (Allow, "never", Never),
            (Polite, "allow", Polite),
            (Polite, "polite", Polite),
            (Polite, "never", Never),
            (Never, "allow", Never),
            (Never, "polite", Never),
            (Never, "never", Never),
        ] {
            assert_eq!(
                tighten_foreground(file, env).unwrap(),
                want,
                "file {file:?} env {env}"
            );
        }
        let err = tighten_foreground(Allow, "nevr").unwrap_err();
        assert!(
            err.contains("AGENTCTL_FOREGROUND") && err.contains("nevr"),
            "{err}"
        );
    }

    #[test]
    fn no_access_profile_and_no_role_loosens_or_sets_foreground() {
        for access in ["ask", "auto", "bypass"] {
            let cfg = PolicyConfig::from_toml_str(&format!(
                "[policy]\naccess = \"{access}\"\n[input]\nforeground = \"never\"\n"
            ))
            .unwrap();
            assert_eq!(cfg.foreground, mcp_types::Foreground::Never, "{access}");
        }
        // A role is a narrowing of tools; it has no foreground setting, and a
        // misspelt role setting is an error rather than a silent no-op.
        let err = PolicyConfig::from_toml_str("[roles.qa2]\nforeground = \"allow\"\n").unwrap_err();
        assert!(err.contains("roles.qa2.foreground"), "{err}");
    }

    #[test]
    fn an_unknown_role_setting_is_an_error() {
        let err =
            PolicyConfig::from_toml_str("[roles.qa2]\ndenied_tool = [\"exec\"]\n").unwrap_err();
        assert!(err.contains("roles.qa2.denied_tool"), "{err}");
    }
}

/// The three levels: how `access` combines with a flag, the environment,
/// `categories`, the legacy keys and dry run; and the spellings, ignored keys
/// and no-op settings the loader now reports.
#[cfg(test)]
mod level_tests {
    use super::*;
    use crate::Access::{Ask, Auto, Bypass};
    use AccessSource::{Env, File, Flag};

    fn load(text: &str) -> PolicyConfig {
        PolicyConfig::from_toml_str(text).unwrap_or_else(|e| panic!("{e}\n{text}"))
    }

    fn with_request(
        text: &str,
        requested: Option<(Access, AccessSource)>,
        dry_run: bool,
    ) -> Result<PolicyConfig, String> {
        let (cfg, facts) = PolicyConfig::parse_config(text)?;
        PolicyConfig::finalize(cfg, &facts, requested, dry_run)
    }

    fn warnings(cfg: &PolicyConfig) -> String {
        cfg.notes.warnings.join("\n")
    }

    /// Every file level against every flag or environment level, from each
    /// source. The rule: toward `ask` is allowed, away from it is an error,
    /// an unset file takes the request, and `bypass` is never taken from the
    /// environment alone.
    #[test]
    fn a_flag_or_environment_level_may_only_tighten_the_files() {
        let files = [None, Some(Ask), Some(Auto), Some(Bypass)];
        let wants = [None, Some(Ask), Some(Auto), Some(Bypass)];
        for file in files {
            for want in wants {
                for via in [Flag, Env] {
                    let got = resolve_access(file, want.map(|w| (w, via)));
                    let label = format!("file {file:?} request {want:?} via {via:?}");
                    match (file, want) {
                        (None, None) => assert_eq!(got, Ok(None), "{label}"),
                        (Some(f), None) => assert_eq!(got, Ok(Some((f, File))), "{label}"),
                        (None, Some(Bypass)) if via == Env => {
                            let e = got.unwrap_err();
                            assert!(
                                e.contains("AGENTCTL_ACCESS=bypass")
                                    && e.contains("config file")
                                    && e.contains("--access bypass"),
                                "{label}: {e}"
                            );
                        }
                        (None, Some(w)) => assert_eq!(got, Ok(Some((w, via))), "{label}"),
                        (Some(f), Some(w)) if w.permissiveness() > f.permissiveness() => {
                            let e = got.unwrap_err();
                            let setter = if via == Env {
                                "AGENTCTL_ACCESS"
                            } else {
                                "--access"
                            };
                            assert!(
                                e.contains(setter)
                                    && e.contains("would loosen")
                                    && e.contains(f.as_str())
                                    && e.contains("toward ask"),
                                "{label}: {e}"
                            );
                        }
                        // The same level is no change, so the file stays the source.
                        (Some(f), Some(w)) if f == w => {
                            assert_eq!(got, Ok(Some((f, File))), "{label}")
                        }
                        (Some(_), Some(w)) => assert_eq!(got, Ok(Some((w, via))), "{label}"),
                    }
                }
            }
        }
    }

    #[test]
    fn the_effective_level_and_where_it_came_from_reach_the_config() {
        let cfg = load("");
        assert_eq!(
            (cfg.access, cfg.notes.access_source),
            (None, AccessSource::Default)
        );
        let cfg = load("[policy]\naccess = \"auto\"\n");
        assert_eq!((cfg.access, cfg.notes.access_source), (Some(Auto), File));
        let cfg = with_request("", Some((Ask, Flag)), false).unwrap();
        assert_eq!((cfg.access, cfg.notes.access_source), (Some(Ask), Flag));
        let cfg =
            with_request("[policy]\naccess = \"bypass\"\n", Some((Auto, Env)), false).unwrap();
        assert_eq!((cfg.access, cfg.notes.access_source), (Some(Auto), Env));
        // The tightened level carries its mode with it.
        assert_eq!(cfg.mode, Mode::Autonomous);
        let cfg = with_request("[policy]\naccess = \"auto\"\n", Some((Ask, Flag)), false).unwrap();
        assert_eq!(cfg.mode, Mode::Interactive);
        // A loosening request is a startup error through the loader too.
        assert!(with_request("[policy]\naccess = \"ask\"\n", Some((Bypass, Flag)), false).is_err());
        assert!(with_request("", Some((Bypass, Env)), false).is_err());
    }

    /// `categories` narrows a level instead of being thrown away; with none
    /// listed every category is on, as before.
    #[test]
    fn categories_narrow_every_level_and_absent_means_all() {
        for level in ["ask", "auto", "bypass"] {
            let cfg = load(&format!(
                "[policy]\naccess = \"{level}\"\ncategories = [\"vision\", \"browser\"]\n"
            ));
            assert_eq!(
                cfg.categories,
                vec![Category::Vision, Category::Browser],
                "{level}"
            );
            let cfg = load(&format!("[policy]\naccess = \"{level}\"\n"));
            assert_eq!(cfg.categories, crate::all_categories(), "{level}");
            // An explicit empty list is a narrowing too: nothing is on.
            let cfg = load(&format!(
                "[policy]\naccess = \"{level}\"\ncategories = []\n"
            ));
            assert!(cfg.categories.is_empty(), "{level}");
        }
        // The same when the level comes from a flag over a file with a list.
        let cfg = with_request(
            "[policy]\ncategories = [\"vision\"]\n",
            Some((Auto, Flag)),
            false,
        )
        .unwrap();
        assert_eq!(cfg.categories, vec![Category::Vision]);
    }

    /// Without `access` nothing about `mode` or `enable` changes, and nothing
    /// is warned about: they are the working spelling there.
    #[test]
    fn the_legacy_keys_work_unchanged_without_access_and_say_nothing() {
        let cfg = load("[policy]\nmode = \"autonomous\"\nenable = [\"power_control\"]\n");
        assert_eq!((cfg.mode, cfg.access), (Mode::Autonomous, None));
        assert_eq!(cfg.enable, vec!["power_control"]);
        assert_eq!(cfg.categories, PolicyConfig::default().categories);
        assert!(cfg.notes.warnings.is_empty() && cfg.notes.unknown_keys.is_empty());
    }

    #[test]
    fn mode_and_enable_beside_access_are_named_as_ignored() {
        for level in [Ask, Auto, Bypass] {
            let cfg = load(&format!(
                "[policy]\naccess = \"{}\"\nmode = \"interactive\"\nenable = []\n",
                level.as_str()
            ));
            assert_eq!(cfg.mode, level.mode(), "the level, not the file's mode");
            let w = warnings(&cfg);
            assert!(
                w.contains("policy.mode is ignored") && w.contains(level.as_str()),
                "{w}"
            );
            assert!(w.contains("policy.enable is ignored"), "{w}");
        }
        // Each alone warns for itself only.
        let w = warnings(&load("[policy]\naccess = \"ask\"\nenable = [\"x\"]\n"));
        assert!(
            w.contains("policy.enable") && !w.contains("policy.mode"),
            "{w}"
        );
        // The warning names where the level came from.
        let cfg = with_request(
            "[policy]\nmode = \"autonomous\"\n",
            Some((Ask, Flag)),
            false,
        )
        .unwrap();
        assert!(
            warnings(&cfg).contains("--access flag"),
            "{}",
            warnings(&cfg)
        );
    }

    /// Dry run is a way to rehearse and works under every level, spelled three
    /// ways; it only ever makes a session do less.
    #[test]
    fn dry_run_applies_under_every_level_and_spelling() {
        for level in [None, Some(Ask), Some(Auto), Some(Bypass)] {
            let access = level.map_or(String::new(), |l| format!("access = \"{}\"\n", l.as_str()));
            for (spelling, flag) in [
                ("dry_run = true\n", false),
                ("mode = \"dry_run\"\n", false),
                ("", true),
            ] {
                let cfg =
                    with_request(&format!("[policy]\n{access}{spelling}"), None, flag).unwrap();
                assert_eq!(cfg.mode, Mode::DryRun, "{level:?} {spelling:?} {flag}");
                assert_eq!(cfg.access, level, "dry run does not change the level");
                // `mode = "dry_run"` beside `access` is not the ignored-key case.
                assert!(
                    !warnings(&cfg).contains("policy.mode"),
                    "{}",
                    warnings(&cfg)
                );
            }
            // Off by default, and `dry_run = false` is no request.
            let cfg = load(&format!("[policy]\n{access}dry_run = false\n"));
            assert_ne!(cfg.mode, Mode::DryRun, "{level:?}");
        }
        // Dry run wins over a stricter-looking or looser-looking mode.
        let cfg = load("[policy]\nmode = \"autonomous\"\ndry_run = true\n");
        assert_eq!(cfg.mode, Mode::DryRun);
        assert!(PolicyConfig::from_toml_str("[policy]\ndry_run = \"yes\"\n").is_err());
    }

    #[test]
    fn an_older_spelling_loads_with_a_warning_naming_the_canonical_key() {
        for (old, canonical, text) in [
            ("input.demo", "policy.demo", "[input]\ndemo = true\n"),
            ("demo.enabled", "policy.demo", "[demo]\nenabled = true\n"),
            (
                "input.demo_speed",
                "policy.demo_speed",
                "[input]\ndemo_speed = \"snappy\"\n",
            ),
            (
                "demo.speed",
                "policy.demo_speed",
                "[demo]\nspeed = \"snappy\"\n",
            ),
            (
                "policy.default_role",
                "policy.role",
                "[policy]\ndefault_role = \"qa\"\n",
            ),
            (
                "policy.invariants.denied_domains",
                "invariants.denied_domains",
                "[policy.invariants]\ndenied_domains = [\"a.example\"]\n",
            ),
            (
                "policy.invariants.protected_paths",
                "invariants.protected_paths",
                "[policy.invariants]\nprotected_paths = [\"/x\"]\n",
            ),
        ] {
            let cfg = load(text);
            let w = warnings(&cfg);
            assert!(
                w.contains(old) && w.contains(&format!("use {canonical}")),
                "{old}: {w}"
            );
        }
        // The values still land.
        assert!(load("[input]\ndemo = true\n").demo);
        assert_eq!(load("[demo]\nspeed = \"snappy\"\n").demo_speed, "snappy");
        assert_eq!(
            load("[policy]\ndefault_role = \"qa\"\n")
                .active_role
                .as_deref(),
            Some("qa")
        );
        assert_eq!(
            load("[policy.invariants]\ndenied_domains = [\"a.example\"]\n")
                .invariants
                .denied_domains,
            vec!["a.example"]
        );
        // The canonical spelling says nothing.
        assert!(load("[policy]\ndemo = true\nrole = \"qa\"\n")
            .notes
            .warnings
            .is_empty());
    }

    #[test]
    fn two_spellings_that_disagree_are_an_error_whatever_the_order() {
        for text in [
            "[policy]\ndemo = false\n[input]\ndemo = true\n",
            "[input]\ndemo = true\n[policy]\ndemo = false\n",
            "[input]\ndemo = true\n[demo]\nenabled = false\n",
            "[policy]\nrole = \"qa\"\ndefault_role = \"readonly\"\n",
            "[policy]\ndemo_speed = \"snappy\"\n[demo]\nspeed = \"cinematic\"\n",
            "[invariants]\ndenied_domains = [\"a.example\"]\n[policy.invariants]\ndenied_domains = [\"b.example\"]\n",
            "[invariants]\nprotected_paths = [\"/a\"]\n[policy.invariants]\nprotected_paths = [\"/b\"]\n",
        ] {
            let err = PolicyConfig::from_toml_str(text).unwrap_err();
            assert!(
                err.contains("two spellings of one setting") && err.contains("keep only"),
                "{err}\n{text}"
            );
        }
        // The same value spelled twice is a warning, not an error, and a list
        // in another order is the same list.
        let cfg = load("[policy]\nrole = \"qa\"\ndefault_role = \"qa\"\n");
        assert_eq!(cfg.active_role.as_deref(), Some("qa"));
        assert_eq!(cfg.notes.warnings.len(), 1);
        load(
            "[invariants]\ndenied_domains = [\"a\", \"b\"]\n[policy.invariants]\ndenied_domains = [\"b\", \"a\"]\n",
        );
        // Deterministic: the same text gives the same result every time.
        let first = PolicyConfig::from_toml_str("[input]\ndemo = true\ndemo_speed = \"off\"\n[policy]\ndemo = true\n[demo]\nspeed = \"off\"\n").unwrap();
        for _ in 0..20 {
            let again = PolicyConfig::from_toml_str("[input]\ndemo = true\ndemo_speed = \"off\"\n[policy]\ndemo = true\n[demo]\nspeed = \"off\"\n").unwrap();
            assert_eq!(again.notes, first.notes);
        }
    }

    #[test]
    fn a_roles_consent_list_has_one_canonical_spelling() {
        let role_of = |text: &str| {
            load(text)
                .roles
                .get("r")
                .cloned()
                .unwrap_or_else(|| panic!("no role r in {text}"))
        };
        // The canonical spelling loads silently and splits tiers from tools.
        let cfg = load("[roles.r]\nrequire_consent_for = [\"dangerous\", \"exec\"]\n");
        assert!(cfg.notes.warnings.is_empty());
        let r = cfg.roles["r"].clone();
        assert_eq!(
            r.require_consent_for_tiers,
            vec![mcp_types::Tier::Dangerous]
        );
        assert_eq!(r.require_consent_for_tools, vec!["exec"]);
        // Each older spelling means what it did, and warns.
        for (spelling, tiers, tools) in [
            (
                "require_consent_for_tiers",
                vec![mcp_types::Tier::Standard],
                vec!["exec"],
            ),
            (
                "require_consent_tiers",
                vec![mcp_types::Tier::Standard],
                vec!["exec"],
            ),
            (
                "require_consent_for_tools",
                vec![],
                vec!["standard", "exec"],
            ),
            ("require_consent_tools", vec![], vec!["standard", "exec"]),
        ] {
            let cfg = load(&format!(
                "[roles.r]\n{spelling} = [\"standard\", \"exec\"]\n"
            ));
            let r = cfg.roles["r"].clone();
            assert_eq!(r.require_consent_for_tiers, tiers, "{spelling}");
            assert_eq!(r.require_consent_for_tools, tools, "{spelling}");
            assert!(
                warnings(&cfg).contains(&format!("roles.r.{spelling}"))
                    && warnings(&cfg).contains("use require_consent_for"),
                "{spelling}: {}",
                warnings(&cfg)
            );
        }
        // Agreeing spellings are fine; disagreeing ones are an error.
        role_of("[roles.r]\nrequire_consent_for = [\"dangerous\"]\nrequire_consent_for_tiers = [\"dangerous\"]\n");
        for bad in [
            "[roles.r]\nrequire_consent_for = [\"dangerous\"]\nrequire_consent_for_tiers = [\"standard\"]\n",
            "[roles.r]\nrequire_consent_for = [\"exec\"]\nrequire_consent_for_tools = [\"browser_eval\"]\n",
            "[roles.r]\nrequire_consent_for_tools = [\"exec\"]\nrequire_consent_tools = [\"pty_spawn\"]\n",
        ] {
            let err = PolicyConfig::from_toml_str(bad).unwrap_err();
            assert!(err.contains("roles.r") && err.contains("keep only"), "{err}");
        }
        assert!(
            PolicyConfig::from_toml_str("[roles.r]\nrequire_consent_for = \"dangerous\"\n")
                .is_err()
        );
    }

    #[test]
    fn max_denials_is_accepted_with_a_warning_that_it_never_did_anything() {
        let cfg = load("[policy]\nmax_denials = 5\n[roles.r]\nmax_denials = 2\n");
        let w = warnings(&cfg);
        assert!(
            w.contains("policy.max_denials has no effect and never had")
                && w.contains("roles.r.max_denials has no effect and never had")
                && w.contains("Remove it"),
            "{w}"
        );
        // Whatever the value, so a config that carries it keeps loading.
        load("[policy]\nmax_denials = 0\n");
        load("[policy]\nmax_denials = \"5\"\n");
        // The role it sits in still exists and is otherwise unchanged.
        assert!(cfg.roles.contains_key("r"));
    }

    #[test]
    fn unknown_keys_are_collected_with_the_nearest_known_key() {
        let cfg = load(
            "[policy]\ncatgories = [\"vision\"]\nfuture_thing = 1\nforeground = \"never\"\n[roles.a.b]\nc = 1\n",
        );
        let by: std::collections::HashMap<_, _> = cfg
            .notes
            .unknown_keys
            .iter()
            .map(|u| (u.key.as_str(), u.suggestion.as_deref()))
            .collect();
        assert_eq!(by["policy.catgories"], Some("policy.categories"));
        assert_eq!(by["policy.future_thing"], None);
        // Right setting, wrong section.
        assert_eq!(by["policy.foreground"], Some("input.foreground"));
        // A role table nested too deep is reported, not dropped in silence.
        assert!(by.contains_key("roles.a.b.c"));
        // And it did not take effect.
        assert_eq!(cfg.categories, PolicyConfig::default().categories);
        assert_eq!(cfg.foreground, mcp_types::Foreground::Allow);
    }

    #[test]
    fn nearest_known_key_suggests_within_a_few_edits_and_otherwise_nothing() {
        for (typo, want) in [
            ("policy.catgories", Some("policy.categories")),
            ("policy.categorys", Some("policy.categories")),
            ("fs.root", Some("fs.roots")),
            (
                "terminal.allowed_command",
                Some("terminal.allowed_commands"),
            ),
            ("policy.allowed_app", Some("policy.allowed_apps")),
            // Unique name, wrong section.
            ("policy.foreground", Some("input.foreground")),
            ("input.allowed_apps", Some("policy.allowed_apps")),
            ("terminal.roots", Some("fs.roots")),
            // The leaf is shared by two sections, so there is no single guess.
            ("network.allowed_origins", None),
            // Nothing close.
            ("policy.future_thing", None),
            ("x.y", None),
            ("", None),
        ] {
            assert_eq!(nearest_known_key(typo), want, "{typo}");
        }
    }

    #[test]
    fn edit_distance_counts_single_character_edits() {
        for (a, b, d) in [
            ("", "", 0),
            ("abc", "abc", 0),
            ("abc", "", 3),
            ("", "ab", 2),
            ("kitten", "sitting", 3),
            ("categories", "catgories", 1),
            ("ab", "ba", 2),
        ] {
            assert_eq!(edit_distance(a, b), d, "{a} {b}");
            assert_eq!(edit_distance(b, a), d, "{b} {a}");
        }
    }

    /// The tables and the loader agree: every alias points at a known key,
    /// every known key is read, aliased or ignored somewhere, and the
    /// documented list leaves out exactly the older spellings and no-ops.
    #[test]
    fn the_key_tables_agree_with_the_loader() {
        for (old, canonical) in ALIASES {
            assert!(KNOWN_KEYS.contains(old), "{old}");
            assert!(KNOWN_KEYS.contains(canonical), "{canonical}");
            assert!(!ALIASES.iter().any(|(o, _)| o == canonical), "{canonical}");
        }
        let src = include_str!("load.rs");
        let start = src.find("for (key, val) in &entries {").unwrap();
        let end = src.find("fn finalize(").unwrap();
        let body = &src[start..end];
        for key in KNOWN_KEYS {
            let read = body.contains(&format!("\"{key}\""));
            let aliased = ALIASES.iter().any(|(o, _)| o == key);
            assert!(read || aliased, "{key} is listed but nothing reads it");
        }
        let canonical = canonical_config_keys();
        assert!(canonical.contains(&"policy.access"));
        assert!(canonical.contains(&"policy.dry_run"));
        assert!(canonical.contains(&"policy.role"));
        for gone in ["policy.max_denials", "input.demo", "policy.default_role"] {
            assert!(!canonical.contains(&gone), "{gone}");
        }
    }

    #[test]
    fn access_from_the_environment_is_validated_and_empty_is_unset() {
        // `access_from_env` reads the process environment, so only the parse
        // that decides its answer is exercised here; the flag-versus-environment
        // table is in `a_flag_or_environment_level_may_only_tighten_the_files`.
        for (v, want) in [
            ("ask", Some(Ask)),
            ("auto", Some(Auto)),
            ("bypass", Some(Bypass)),
        ] {
            assert_eq!(Access::parse(v), want);
        }
        assert_eq!(Access::parse("ASK"), None);
        assert_eq!(Access::parse(""), None);
        assert_eq!(Access::parse("yolo"), None);
    }
}
