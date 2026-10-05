use std::collections::HashMap;
use std::path::{Path, PathBuf};

use mcp_types::{Category, Tier, ToolDescriptor};
use serde::{Deserialize, Serialize};

/// Role Profile for Enterprise Role-Based Access Control (RBAC).
/// Constrains tool execution by tier, category, specific tool allow/deny lists,
/// and consent requirements.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleProfile {
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_tiers: Option<Vec<Tier>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_categories: Option<Vec<Category>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denied_categories: Vec<Category>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denied_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub require_consent_for_tiers: Vec<Tier>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub require_consent_for_tools: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_denials: Option<usize>,
}

impl RoleProfile {
    pub fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        RoleProfile {
            name,
            description: String::new(),
            allowed_tiers: None,
            allowed_categories: None,
            denied_categories: Vec::new(),
            allowed_tools: None,
            denied_tools: Vec::new(),
            require_consent_for_tiers: Vec::new(),
            require_consent_for_tools: Vec::new(),
            max_denials: None,
        }
    }

    /// Read-only inspection & compliance auditor profile.
    /// Strictly limits execution to Tier::Read, forbidding all state mutations and shell commands.
    pub fn readonly() -> Self {
        RoleProfile {
            name: "readonly".to_string(),
            description: "Read-only inspection and monitoring (no state mutation or OS execution)"
                .to_string(),
            allowed_tiers: Some(vec![Tier::Read]),
            allowed_categories: None,
            denied_categories: vec![Category::Terminal, Category::Credentials, Category::Memory],
            allowed_tools: None,
            denied_tools: vec![
                "browser_act".into(),
                "browser_fill_form".into(),
                "browser_eval".into(),
                "ui_action".into(),
                "ui_fill_form".into(),
                "keyboard_type".into(),
                "keyboard_shortcut".into(),
                "mouse_action".into(),
                "drag_drop".into(),
                "set_value".into(),
            ],
            require_consent_for_tiers: Vec::new(),
            require_consent_for_tools: Vec::new(),
            max_denials: Some(5),
        }
    }

    /// QA / Test Engineer profile.
    /// Full browser and UI testing capabilities, with shell and credential access forbidden.
    pub fn qa() -> Self {
        RoleProfile {
            name: "qa".to_string(),
            description: "QA automation and browser verification (no shell or credential access)"
                .to_string(),
            allowed_tiers: None,
            allowed_categories: Some(vec![
                Category::Browser,
                Category::Vision,
                Category::Window,
                Category::System,
                Category::Input,
            ]),
            denied_categories: vec![
                Category::Terminal,
                Category::Credentials,
                Category::Memory,
                Category::Packages,
            ],
            allowed_tools: None,
            denied_tools: vec!["browser_eval".into()],
            require_consent_for_tiers: Vec::new(),
            require_consent_for_tools: Vec::new(),
            max_denials: None,
        }
    }

    /// Browser-only profile: advertises and permits just the browser engine's
    /// tools, so a web QA or demo session isn't handed the whole tool surface.
    /// Narrows only: dangerous browser tools still need `access` or `enable`.
    pub fn browser() -> Self {
        RoleProfile {
            name: "browser".to_string(),
            description: "Browser automation only (no desktop, shell, files or network tools)"
                .to_string(),
            allowed_tiers: None,
            allowed_categories: Some(vec![Category::Browser]),
            denied_categories: Vec::new(),
            allowed_tools: None,
            denied_tools: Vec::new(),
            require_consent_for_tiers: Vec::new(),
            require_consent_for_tools: Vec::new(),
            max_denials: None,
        }
    }

    /// Standard Human-in-the-Loop Operator profile.
    /// General operations permitted; dangerous actions require interactive consent.
    pub fn operator() -> Self {
        RoleProfile {
            name: "operator".to_string(),
            description: "Standard human-in-the-loop operator (dangerous actions require consent)"
                .to_string(),
            allowed_tiers: None,
            allowed_categories: None,
            denied_categories: Vec::new(),
            allowed_tools: None,
            denied_tools: Vec::new(),
            require_consent_for_tiers: vec![Tier::Dangerous],
            require_consent_for_tools: Vec::new(),
            max_denials: None,
        }
    }

    /// Full administrative privileges.
    pub fn admin() -> Self {
        RoleProfile {
            name: "admin".to_string(),
            description: "Full system administrative privileges".to_string(),
            allowed_tiers: None,
            allowed_categories: None,
            denied_categories: Vec::new(),
            allowed_tools: None,
            denied_tools: Vec::new(),
            require_consent_for_tiers: Vec::new(),
            require_consent_for_tools: Vec::new(),
            max_denials: None,
        }
    }

    /// Check whether this role permits calling the given tool descriptor.
    /// Returns Ok(()) if permitted, or Err(reason) explaining the policy violation.
    pub fn check_permission(&self, desc: &ToolDescriptor) -> Result<(), String> {
        // 1. Check allowed categories
        if let Some(allowed) = &self.allowed_categories {
            if !allowed.contains(&desc.category) {
                return Err(format!(
                    "role '{}' does not permit category '{}'",
                    self.name,
                    desc.category.slug()
                ));
            }
        }

        // 2. Check denied categories
        if self.denied_categories.contains(&desc.category) {
            return Err(format!(
                "role '{}' explicitly forbids category '{}'",
                self.name,
                desc.category.slug()
            ));
        }

        // 3. Check allowed tiers
        if let Some(allowed) = &self.allowed_tiers {
            if !allowed.contains(&desc.tier) {
                return Err(format!(
                    "role '{}' restricts execution to {:?} tier (tool '{}' is {:?})",
                    self.name, allowed, desc.name, desc.tier
                ));
            }
        }

        // 4. Check denied tools
        if self.denied_tools.iter().any(|t| t == &desc.name) {
            return Err(format!(
                "tool '{}' is explicitly denied by role '{}'",
                desc.name, self.name
            ));
        }

        // 5. Check allowed tools (if specified as a strict whitelist)
        if let Some(allowed) = &self.allowed_tools {
            if !allowed.iter().any(|t| t == &desc.name) {
                return Err(format!(
                    "role '{}' does not allow tool '{}' (not in role allowlist)",
                    self.name, desc.name
                ));
            }
        }

        Ok(())
    }

    /// Whether this role requires human consent for this tool call.
    pub fn requires_consent(&self, desc: &ToolDescriptor) -> bool {
        self.require_consent_for_tiers.contains(&desc.tier)
            || self
                .require_consent_for_tools
                .iter()
                .any(|t| t == &desc.name)
    }

    /// Whether this tool descriptor is allowed to be visible in tools listing for this role.
    pub fn allows_tool(&self, desc: &ToolDescriptor) -> bool {
        self.check_permission(desc).is_ok()
    }
}

/// Hard policy invariants: protected paths and denied domains, checked against
/// every call's arguments before the gate, so `access = "bypass"` does not
/// turn them off.
///
/// They are a heuristic over argument text, not containment. They catch a
/// protected path or denied host written out in an argument, including inside
/// a command line, after `..` and symlink resolution; they cannot see what a
/// shell expands, what a relative path resolves to in another process's
/// working directory, or where a host name resolves. Those gaps are pinned in
/// `documented_known_bypasses` in `tests/rbac_invariants.rs`. The real limits
/// on an agent stay the filesystem jail, the command allowlist and the SSRF
/// guard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HardInvariants {
    pub protected_paths: Vec<PathBuf>,
    pub denied_domains: Vec<String>,
}

impl Default for HardInvariants {
    fn default() -> Self {
        HardInvariants {
            protected_paths: default_protected_paths(),
            denied_domains: Vec::new(),
        }
    }
}

/// System paths protected by default.
pub fn default_protected_paths() -> Vec<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        vec![
            PathBuf::from(r"C:\Windows\System32"),
            PathBuf::from(r"C:\Windows\SysWOW64"),
        ]
    }
    #[cfg(not(target_os = "windows"))]
    {
        vec![
            PathBuf::from("/etc/shadow"),
            PathBuf::from("/etc/sudoers"),
            PathBuf::from("/etc/pam.d"),
            PathBuf::from("/System"),
            PathBuf::from("/private/etc/master.passwd"),
            PathBuf::from("/etc/master.passwd"),
        ]
    }
}

/// Lexically normalise a path: drop `.`, apply `..`. No filesystem access.
fn lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            std::path::Component::RootDir => out.push(std::path::MAIN_SEPARATOR_STR),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::Normal(c) => out.push(c),
        }
    }
    out
}

/// The forms a path can be compared in: as written (normalised) and, when
/// the longest existing prefix resolves through a symlink, as resolved. On
/// macOS `/etc` is a link to `/private/etc`, so `/private/etc/sudoers` and
/// `/etc/sudoers` must be recognised as the same file.
fn path_forms(p: &Path) -> Vec<PathBuf> {
    let lex = lexical(p);
    let mut forms = vec![lex.clone()];
    let mut existing = lex.as_path();
    let mut rest: Vec<&std::ffi::OsStr> = Vec::new();
    loop {
        if let Ok(real) = std::fs::canonicalize(existing) {
            let mut full = real;
            for part in rest.iter().rev() {
                full.push(part);
            }
            if full != lex {
                forms.push(full);
            }
            break;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name);
                existing = parent;
            }
            _ => break,
        }
    }
    forms
}

/// Whether `candidate` is, or is inside, a protected path, comparing both the
/// written and the symlink-resolved forms of each side.
pub fn is_path_protected(candidate: &str, protected: &[PathBuf]) -> bool {
    let cand = path_forms(Path::new(candidate));
    protected.iter().any(|prot| {
        let prots = path_forms(prot);
        cand.iter()
            .any(|c| prots.iter().any(|p| c == p || c.starts_with(p)))
    })
}

/// The host of a URL or bare host string: scheme case-insensitive, userinfo
/// (`user@`) stripped, port and path dropped, trailing dot removed, lowercased.
fn host_of(url_or_host: &str) -> String {
    let s = url_or_host.trim();
    let after_scheme = match s.find("://") {
        Some(i) => &s[i + 3..],
        None => s.strip_prefix("//").unwrap_or(s),
    };
    let authority = after_scheme
        .split(['/', '?', '#', '\\'])
        .next()
        .unwrap_or("");
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    let host = if let Some(rest) = host_port.strip_prefix('[') {
        rest.split(']').next().unwrap_or(rest)
    } else {
        host_port.split(':').next().unwrap_or(host_port)
    };
    host.trim_end_matches('.').to_ascii_lowercase()
}

/// Whether a URL or host names a denied domain or one of its subdomains.
pub fn is_domain_denied(url_or_host: &str, denied_domains: &[String]) -> bool {
    if denied_domains.is_empty() {
        return false;
    }
    let target = host_of(url_or_host);
    if target.is_empty() {
        return false;
    }
    denied_domains.iter().any(|d| {
        let denied = d.trim().trim_end_matches('.').to_ascii_lowercase();
        !denied.is_empty() && (target == denied || target.ends_with(&format!(".{denied}")))
    })
}

/// Argument keys whose bare value is a host name rather than free text.
const HOST_KEYS: &[&str] = &["host", "hostname", "domain", "server", "origin"];

fn looks_like_url(token: &str) -> bool {
    match token.find("://") {
        Some(i) => {
            let scheme = &token[..i];
            !scheme.is_empty()
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        }
        None => false,
    }
}

fn file_url_path(token: &str) -> Option<String> {
    let lower = token.to_ascii_lowercase();
    if !lower.starts_with("file://") {
        return None;
    }
    let rest = &token[7..];
    // file:///etc/x or file://localhost/etc/x
    let i = rest.find('/')?;
    Some(rest[i..].to_string())
}

fn looks_like_path(token: &str) -> bool {
    token.starts_with('/') || token.starts_with('\\') || token.contains(":\\")
}

/// Check one string: the whole value and each whitespace/quote-separated
/// token, so a path inside a command line (`cat /etc/shadow`) is seen too.
fn check_string(s: &str, key: Option<&str>, inv: &HardInvariants) -> Result<(), String> {
    let tokens = std::iter::once(s.trim()).chain(
        s.split(|c: char| {
            c.is_whitespace()
                || matches!(
                    c,
                    '"' | '\'' | '`' | ';' | '|' | '&' | '(' | ')' | '<' | '>' | '='
                )
        })
        .filter(|t| !t.is_empty()),
    );
    for tok in tokens {
        if let Some(p) = file_url_path(tok) {
            if is_path_protected(&p, &inv.protected_paths) {
                return Err(format!("path '{p}' (from '{tok}') is protected"));
            }
        } else if looks_like_url(tok) {
            if is_domain_denied(tok, &inv.denied_domains) {
                return Err(format!("domain access in '{tok}' is forbidden"));
            }
        } else if looks_like_path(tok) && is_path_protected(tok, &inv.protected_paths) {
            return Err(format!("path '{tok}' is protected"));
        }
    }
    if key.is_some_and(|k| HOST_KEYS.contains(&k)) && is_domain_denied(s, &inv.denied_domains) {
        return Err(format!("host '{s}' is forbidden"));
    }
    Ok(())
}

fn check_value(
    v: &serde_json::Value,
    key: Option<&str>,
    inv: &HardInvariants,
) -> Result<(), String> {
    match v {
        serde_json::Value::String(s) => check_string(s, key, inv),
        serde_json::Value::Array(arr) => {
            arr.iter().try_for_each(|item| check_value(item, key, inv))
        }
        serde_json::Value::Object(map) => map
            .iter()
            .try_for_each(|(k, item)| check_value(item, Some(k.as_str()), inv)),
        _ => Ok(()),
    }
}

/// Scan a call's arguments, at any depth, for protected paths and denied
/// domains.
pub fn check_arguments_invariants(
    args: &serde_json::Value,
    invariants: &HardInvariants,
) -> Result<(), String> {
    check_value(args, None, invariants)
        .map_err(|why| format!("hard policy invariant violation: {why}"))
}

/// Helper resolving a role name against built-in roles and custom configuration.
pub fn resolve_role_profile(
    name: &str,
    custom_roles: &HashMap<String, RoleProfile>,
) -> Option<RoleProfile> {
    if let Some(role) = custom_roles.get(name) {
        return Some(role.clone());
    }
    match name.to_ascii_lowercase().as_str() {
        "readonly" | "auditor" => Some(RoleProfile::readonly()),
        "qa" | "tester" => Some(RoleProfile::qa()),
        "browser" => Some(RoleProfile::browser()),
        "operator" => Some(RoleProfile::operator()),
        "admin" | "superadmin" => Some(RoleProfile::admin()),
        _ => None,
    }
}
