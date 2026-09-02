use std::path::PathBuf;

use mcp_types::Category;

/// How denied-but-consentable actions are handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Prompt a human on `NeedConsent`.
    Interactive,
    /// No consent channel: `NeedConsent` becomes a denial.
    Autonomous,
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
    /// Apps that `launch` (and app-targeted actions) are allowed to touch.
    pub allowed_apps: Vec<String>,
    /// URL prefixes the browser engine may navigate to. Empty = no restriction.
    pub allowed_origins: Vec<String>,
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
    /// stderr — better than a memorable default nobody changes.
    pub http_token: String,
    /// Browser origins allowed to call the endpoint. Empty = any request
    /// carrying an `Origin` header is refused.
    pub http_allowed_origins: Vec<String>,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        let base = default_agentctl_dir();
        PolicyConfig {
            categories: vec![Category::Vision, Category::Input, Category::Window],
            enable: Vec::new(),
            mode: Mode::Interactive,
            allowed_apps: ["Terminal", "Finder", "TextEdit"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            allowed_origins: Vec::new(),
            max_denials: 5,
            max_consent_prompts: 20,
            kill_switch_file: base.join("STOP"),
            audit_dir: base.join("audit"),
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
