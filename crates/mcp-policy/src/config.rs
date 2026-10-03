use std::path::PathBuf;

use mcp_types::Category;

/// How denied-but-consentable actions are handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Prompt a human on `NeedConsent`.
    Interactive,
    /// No consent channel: `NeedConsent` becomes a denial.
    Autonomous,
    /// Rehearsal. Read-tier tools run normally; anything that would change
    /// something reports what it *would* have done and does not do it.
    ///
    /// This exists because the only way to find out what an agent will do to a
    /// real machine was to let it. A prompt can now be exercised against the
    /// real config, the real tool list and the real gate, with nothing at risk.
    DryRun,
}

impl Mode {
    /// The config spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Interactive => "interactive",
            Mode::Autonomous => "autonomous",
            Mode::DryRun => "dry_run",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "interactive" => Some(Mode::Interactive),
            "autonomous" => Some(Mode::Autonomous),
            "dry_run" | "dry-run" => Some(Mode::DryRun),
            _ => None,
        }
    }

    /// Whether a human can be asked. Only interactive mode has a channel;
    /// dry run must not prompt, because nothing is going to happen anyway and
    /// a dialog would train the operator to approve rehearsals.
    pub fn prompts_human(self) -> bool {
        matches!(self, Mode::Interactive)
    }
}

/// A one-word permission profile that spares the operator the chore of
/// enabling each category and naming each dangerous tool. Setting it turns on
/// every capability and picks how risk is handled; the granular fields
/// (`categories`, `enable`) are then unnecessary and ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Read and act freely; a dangerous tool or a high-impact action asks the
    /// human through the consent dialog. The safe default profile.
    Ask,
    /// Run unattended: nothing prompts, but a clearly destructive action is
    /// refused rather than done blind (there is no human to approve it).
    Auto,
    /// Everything runs, nothing prompts, the destructive gate is off. Only the
    /// kill switch and human-override remain. The operator's explicit "I take
    /// responsibility"; never a default.
    Bypass,
}

impl Access {
    pub fn as_str(self) -> &'static str {
        match self {
            Access::Ask => "ask",
            Access::Auto => "auto",
            Access::Bypass => "bypass",
        }
    }

    pub fn parse(s: &str) -> Option<Access> {
        match s {
            "ask" => Some(Access::Ask),
            "auto" => Some(Access::Auto),
            "bypass" => Some(Access::Bypass),
            _ => None,
        }
    }

    /// The interaction mode this profile implies.
    pub fn mode(self) -> Mode {
        match self {
            Access::Ask => Mode::Interactive,
            Access::Auto | Access::Bypass => Mode::Autonomous,
        }
    }
}

/// Every capability category, for the `access` profiles that enable them all.
pub fn all_categories() -> Vec<Category> {
    use Category::*;
    vec![
        Vision,
        Input,
        Window,
        Terminal,
        Filesystem,
        Network,
        System,
        Credentials,
        Memory,
        Desktop,
        Browser,
        Packages,
    ]
}

/// The policy configuration. Secure defaults: only the three GUI categories are
/// enabled, no dangerous tools are opted in, interactive consent.
#[derive(Debug, Clone)]
pub struct PolicyConfig {
    /// Enabled categories (the coarse gate). Only these are advertised/callable.
    pub categories: Vec<Category>,
    /// Dangerous tools opted in by exact name.
    pub enable: Vec<String>,
    pub mode: Mode,
    /// The one-word permission profile. When `Some`, it enables all categories
    /// and all dangerous tools and sets `mode`; the granular fields are ignored.
    pub access: Option<Access>,
    /// Apps that `launch` (and app-targeted actions) are allowed to touch.
    pub allowed_apps: Vec<String>,
    /// Origins the browser engine may navigate to (`scheme://host[:port][/path]`).
    /// Empty = any public origin. Either way the resolved address is checked,
    /// so loopback, private and cloud-metadata targets need `allow_private`.
    pub allowed_origins: Vec<String>,
    /// Permit `browser_navigate` to loopback/private/link-local targets, for
    /// driving a local development server. Off by default: the browser is a
    /// network client inside the perimeter, same as `http_request`.
    pub browser_allow_private: bool,
    /// The System One judge (`[judge]`). Off by default; when on, a judgment
    /// may tighten a decision or rank candidates, never loosen anything.
    pub judge: mcp_judge::JudgeConfig,
    /// Session aborts after this many denied calls (anti-spin).
    pub max_denials: usize,
    /// How many times one session may interrupt a human for approval. Past this
    /// every further request is denied without prompting, so an agent cannot
    /// train the operator to click Allow (consent fatigue).
    pub max_consent_prompts: usize,
    /// Presence of this file trips the kill switch.
    pub kill_switch_file: PathBuf,
    /// Directory for `<session>.jsonl` audit logs.
    pub audit_dir: PathBuf,
    /// File holding the Ed25519 seed that signs audit logs (`agentctl audit
    /// keygen`). Unset, each session signs with a throwaway key, and a log can
    /// only be checked for self-consistency, not attributed to this operator.
    pub audit_signing_key: Option<PathBuf>,
    /// Directories the filesystem engine may touch. Empty = it refuses all.
    pub fs_roots: Vec<PathBuf>,
    /// Binaries `exec` may run. Empty = it runs nothing.
    pub allowed_commands: Vec<String>,
    /// Permit `exec` with `shell: true`.
    pub allow_shell: bool,
    /// Hosts the network engine may reach. Empty = it reaches nothing.
    pub allowed_hosts: Vec<String>,
    /// Permit private/loopback network targets.
    pub allow_private_network: bool,
    /// Keychain services the credentials engine may touch. Empty = none.
    pub allowed_services: Vec<String>,
    /// Apps whose typed input is treated as shell input and screened for
    /// destructive commands. Includes editors, because their integrated
    /// terminals run the same shell.
    pub terminal_apps: Vec<String>,
    /// Stop when a human takes over the mouse. See `mcp_input::human_override`:
    /// reaching for the mouse is the reflex people already have for
    /// interrupting something, so it is the interrupt worth honouring.
    pub human_override: bool,
    /// How far the pointer must be from anywhere the server put it, in points.
    pub human_override_px: u32,
    /// How long after an action the server still counts as driving.
    pub human_override_grace_ms: u64,
    /// Shells `pty_spawn` may start, by absolute path.
    pub allowed_shells: Vec<String>,
    pub max_pty_sessions: usize,
    pub max_pty_buffer: usize,
    /// Package managers the packages engine may use. Empty = it refuses all.
    pub allowed_sources: Vec<String>,
    /// Permit installing from a URL, local file or third-party tap.
    pub allow_arbitrary_source: bool,
    /// If non-empty, only these package ids may be installed.
    pub package_allowlist: Vec<String>,
    pub package_denylist: Vec<String>,
    /// Where recorded sequences live.
    pub memory_store: PathBuf,
    pub max_recipes: usize,
    // Screen-capture tunables. These are cost/legibility trade-offs rather than
    // security controls, but they belong with the rest of the operator's
    // configuration; `agentctl` maps them into `mcp_vision::VisionConfig`.
    // Held as plain numbers because `mcp-policy` sits *below* every engine and
    // must not depend on one.
    pub vision_detail_low_px: u32,
    pub vision_detail_balanced_px: u32,
    pub vision_detail_full_px: u32,
    /// One of `low`, `balanced`/`medium`, `full`/`high`; validated at load.
    pub vision_default_detail: String,
    pub vision_unchanged_mad: f64,
    pub vision_pixels_per_token: u32,
    pub vision_max_image_bytes: usize,
    /// Serve over HTTP instead of stdio. Off by default: stdio needs no
    /// authentication because the client *is* the parent process, while a
    /// listening socket has to earn that trust back.
    pub http_enabled: bool,
    /// Listen address. Loopback only; the transport refuses anything else.
    pub http_bind: String,
    /// Bearer token. Empty means one is generated at startup and printed to
    /// stderr, better than a memorable default nobody changes.
    pub http_token: String,
    /// Browser origins allowed to call the endpoint. Empty = any request
    /// carrying an `Origin` header is refused.
    pub http_allowed_origins: Vec<String>,
    /// Replace PII/PHI in tool results with synthetic tokens (`<SSN_1>`) before
    /// the model sees them, and restore them only inside local input tools.
    /// Off by default: it rewrites every result, including IP addresses and
    /// phone-like numbers in network and system output.
    pub anonymize: bool,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        let base = default_agentctl_dir();
        PolicyConfig {
            categories: vec![Category::Vision, Category::Input, Category::Window],
            enable: Vec::new(),
            mode: Mode::Interactive,
            access: None,
            anonymize: false,
            allowed_apps: ["Terminal", "Finder", "TextEdit"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            allowed_origins: Vec::new(),
            browser_allow_private: false,
            judge: mcp_judge::JudgeConfig::default(),
            max_denials: 5,
            max_consent_prompts: 20,
            kill_switch_file: base.join("STOP"),
            audit_dir: base.join("audit"),
            audit_signing_key: None,
            // Every commodity engine defaults to *closed*: no roots, no
            // runnable binaries, no reachable hosts, no keychain services.
            // An operator opts in explicitly via config.toml.
            fs_roots: Vec::new(),
            allowed_commands: Vec::new(),
            allow_shell: false,
            allowed_hosts: Vec::new(),
            allow_private_network: false,
            allowed_services: Vec::new(),
            terminal_apps: mcp_input_terminal_apps(),
            human_override: true,
            human_override_px: 12,
            human_override_grace_ms: 1_500,
            allowed_shells: ["/bin/zsh", "/bin/bash", "/bin/sh"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            max_pty_sessions: 4,
            max_pty_buffer: 256 * 1024,
            // Closed, like every other engine: naming no manager means the
            // packages engine refuses everything rather than defaulting to one.
            allowed_sources: Vec::new(),
            allow_arbitrary_source: false,
            package_allowlist: Vec::new(),
            package_denylist: Vec::new(),
            memory_store: base.join("memory.json"),
            max_recipes: 500,
            vision_detail_low_px: 768,
            vision_detail_balanced_px: 1024,
            vision_detail_full_px: 1568,
            vision_default_detail: "full".to_string(),
            vision_unchanged_mad: 1.0,
            vision_pixels_per_token: 750,
            vision_max_image_bytes: 8_000_000,
            http_enabled: false,
            http_bind: "127.0.0.1:8765".to_string(),
            http_token: String::new(),
            http_allowed_origins: Vec::new(),
        }
    }
}

/// The default terminal-app list.
///
/// Duplicated here rather than imported: `mcp-policy` sits *below* the engines
/// and must not depend on one of them. Kept in sync by a test in `mcp-input`
/// that asserts the two lists agree.
fn mcp_input_terminal_apps() -> Vec<String> {
    [
        "Terminal",
        "iTerm",
        "Warp",
        "Alacritty",
        "kitty",
        "Ghostty",
        "WezTerm",
        "Hyper",
        "Tabby",
        "Code",
        "Visual Studio Code",
        "VSCodium",
        "Cursor",
        "Windsurf",
        "Zed",
        "Sublime Text",
        "JetBrains",
        "IntelliJ",
        "PyCharm",
        "WebStorm",
        "RustRover",
        "Xcode",
        "Nova",
        "Emacs",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// `~/.agentctl` (or `./.agentctl` if `$HOME` is unset). Avoids a `dirs`
/// dependency to keep the build lean.
pub fn default_agentctl_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".agentctl")
}

impl PolicyConfig {
    /// The whole effective policy as JSON, with the HTTP token redacted.
    ///
    /// `agentctl config print` only ever showed `[policy]`, which is the half
    /// an operator is least likely to get wrong. The parts that decide what the
    /// agent can actually reach (roots, commands, hosts, services) were
    /// invisible.
    pub fn to_redacted_json(&self) -> serde_json::Value {
        use serde_json::json;
        let paths = |v: &[std::path::PathBuf]| -> Vec<String> {
            v.iter().map(|p| p.display().to_string()).collect()
        };
        json!({
            "policy": {
                "categories": self.categories.iter().map(|c| c.slug()).collect::<Vec<_>>(),
                "enable": self.enable,
                "mode": self.mode.as_str(),
                "access": self.access.map(|a| a.as_str()),
                "anonymize": self.anonymize,
                "allowed_apps": self.allowed_apps,
                "max_denials": self.max_denials,
                "max_consent_prompts": self.max_consent_prompts,
                "kill_switch_file": self.kill_switch_file.display().to_string(),
                "audit_dir": self.audit_dir.display().to_string(),
                "audit_signing_key": self.audit_signing_key.as_ref().map(|p| p.display().to_string()),
            },
            "input": {
                "terminal_apps": self.terminal_apps,
                "human_override": self.human_override,
                "human_override_px": self.human_override_px,
                "human_override_grace_ms": self.human_override_grace_ms,
            },
            "fs": { "roots": paths(&self.fs_roots) },
            "terminal": {
                "allowed_commands": self.allowed_commands,
                "allow_shell": self.allow_shell,
                "allowed_shells": self.allowed_shells,
                "max_pty_sessions": self.max_pty_sessions,
                "max_pty_buffer": self.max_pty_buffer,
            },
            "network": {
                "allowed_hosts": self.allowed_hosts,
                "allow_private": self.allow_private_network,
            },
            "credentials": { "allowed_services": self.allowed_services },
            "packages": {
                "allowed_sources": self.allowed_sources,
                "allow_arbitrary_source": self.allow_arbitrary_source,
                "allowlist": self.package_allowlist,
                "denylist": self.package_denylist,
            },
            "browser": {
                "allowed_origins": self.allowed_origins,
                "allow_private": self.browser_allow_private,
            },
            "judge": {
                "enabled": self.judge.enabled,
                "base_url": self.judge.base_url,
                "model": self.judge.model,
                "timeout_ms": self.judge.timeout_ms,
                "threshold": self.judge.threshold,
                "max_state_bytes": self.judge.max_state_bytes,
            },
            "memory": {
                "store": self.memory_store.display().to_string(),
                "max_recipes": self.max_recipes,
            },
            "http": {
                "enabled": self.http_enabled,
                "allowed_origins": self.http_allowed_origins,
                // Never the value: anyone who can read this can use the transport.
                "token": if self.http_token.is_empty() {
                    "(generated per session)".to_string()
                } else {
                    format!("‹redacted:len={}›", self.http_token.len())
                },
            },
        })
    }
}

#[cfg(test)]
mod mode_tests {
    use super::Mode;

    #[test]
    fn every_mode_round_trips_through_its_config_spelling() {
        for m in [Mode::Interactive, Mode::Autonomous, Mode::DryRun] {
            assert_eq!(Mode::parse(m.as_str()), Some(m));
        }
        // A hyphen is the obvious typo for the underscore spelling.
        assert_eq!(Mode::parse("dry-run"), Some(Mode::DryRun));
        assert_eq!(Mode::parse("nonsense"), None);
    }

    /// Only interactive mode has anyone to ask. Dry run must not prompt: there
    /// is nothing to approve, and asking would teach the operator to approve.
    #[test]
    fn only_interactive_mode_prompts_a_human() {
        assert!(Mode::Interactive.prompts_human());
        assert!(!Mode::Autonomous.prompts_human());
        assert!(!Mode::DryRun.prompts_human());
    }
}
