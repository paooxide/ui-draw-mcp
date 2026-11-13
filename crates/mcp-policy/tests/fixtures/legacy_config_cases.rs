/// Representative configs written the way they were before `access` became the
/// one permission choice: none of them sets `access`.
pub const CASES: &[(&str, &str)] = &[
    ("empty", ""),
    (
        "typical_interactive",
        r#"
[policy]
categories = ["vision", "input", "window", "browser", "system"]
enable = ["browser_eval"]
mode = "interactive"
allowed_apps = ["TextEdit", "Finder"]
max_consent_prompts = 7
max_denials = 3
anonymize = true
"#,
    ),
    (
        "autonomous_with_scope_lists",
        r#"
[policy]
categories = ["vision", "terminal", "filesystem", "network"]
enable = ["power_control"]
mode = "autonomous"
[fs]
roots = ["/tmp/work"]
[terminal]
allowed_commands = ["git", "ls"]
allow_shell = false
[network]
allowed_hosts = [".example.com"]
"#,
    ),
    (
        "dry_run_rehearsal",
        r#"
[policy]
categories = ["vision", "browser"]
enable = ["browser_eval", "browser_cookies"]
mode = "dry_run"
"#,
    ),
    (
        "older_spellings",
        r#"
[policy]
default_role = "operator"
categories = ["vision", "browser"]
max_denials = 4
[input]
demo = true
demo_speed = "snappy"
[policy.invariants]
protected_paths = ["/etc/shadow", "/Users/x/.ssh"]
denied_domains = ["pastebin.com"]
[roles.custom]
description = "mine"
allowed_categories = ["browser"]
denied_tools = ["browser_eval"]
require_consent_for_tiers = ["standard", "browser_cookies"]
max_denials = 2
"#,
    ),
    (
        "kitchen_sink",
        r#"
[policy]
categories = ["vision", "input", "window", "browser", "system", "desktop"]
mode = "autonomous"
role = "readonly"
audit_dir = "/tmp/audit"
kill_switch_file = "/tmp/STOP"
[demo]
enabled = true
speed = "cinematic"
[invariants]
denied_domains = ["evil.example"]
[input]
foreground = "polite"
polite_idle_ms = 900
human_override = false
[browser]
allowed_origins = ["https://ok.example"]
allow_private = true
[packages]
allowed_sources = ["brew"]
allowlist = ["jq"]
[judge]
enabled = false
threshold = 0.6
[http]
enabled = true
token = "secret-token"
[vision]
detail_low_px = 640
[memory]
max_recipes = 12
"#,
    ),
];
