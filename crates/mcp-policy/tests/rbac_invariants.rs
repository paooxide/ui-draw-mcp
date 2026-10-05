use mcp_policy::{
    check_arguments_invariants, is_domain_denied, is_path_protected, parse_audit_records,
    verify_audit_log, AuditRecord, AuditSink, ComplianceExporter, Decision, HardInvariants, Policy,
    PolicyConfig, RoleProfile,
};
use mcp_types::{Category, Tier, ToolDescriptor};
use serde_json::json;

fn make_tool(name: &'static str, cat: Category, tier: Tier) -> ToolDescriptor {
    ToolDescriptor::new(name, cat, tier, "test tool", json!({}))
}

#[test]
fn test_builtin_readonly_role() {
    let role = RoleProfile::readonly();
    assert_eq!(role.name, "readonly");

    let read_tool = make_tool("browser_snapshot", Category::Browser, Tier::Read);
    let standard_tool = make_tool("browser_navigate", Category::Browser, Tier::Standard);
    let branch_tool = make_tool("browser_branch", Category::Browser, Tier::Standard);
    let checkpoint_tool = make_tool("browser_checkpoint", Category::Browser, Tier::Standard);
    let dangerous_tool = make_tool("browser_eval", Category::Browser, Tier::Dangerous);

    assert!(role.allows_tool(&read_tool));
    assert!(!role.allows_tool(&standard_tool));
    assert!(!role.allows_tool(&branch_tool));
    assert!(!role.allows_tool(&checkpoint_tool));
    assert!(!role.allows_tool(&dangerous_tool));

    assert!(role.check_permission(&read_tool).is_ok());
    assert!(role.check_permission(&standard_tool).is_err());
    assert!(role.check_permission(&branch_tool).is_err());
    assert!(role.check_permission(&checkpoint_tool).is_err());
    assert!(role.check_permission(&dangerous_tool).is_err());
}

#[test]
fn test_builtin_qa_role() {
    let role = RoleProfile::qa();
    assert_eq!(role.name, "qa");

    let nav_tool = make_tool("browser_navigate", Category::Browser, Tier::Standard);
    let branch_tool = make_tool("browser_branch", Category::Browser, Tier::Standard);
    let checkpoint_tool = make_tool("browser_checkpoint", Category::Browser, Tier::Standard);
    let click_tool = make_tool("mouse_action", Category::Input, Tier::Standard);
    let eval_tool = make_tool("browser_eval", Category::Browser, Tier::Dangerous);
    let pty_tool = make_tool("pty_spawn", Category::Terminal, Tier::Dangerous);
    let sec_tool = make_tool("creds_read", Category::Credentials, Tier::Read);

    assert!(role.allows_tool(&nav_tool));
    assert!(role.allows_tool(&branch_tool));
    assert!(role.allows_tool(&checkpoint_tool));
    assert!(role.allows_tool(&click_tool));
    assert!(!role.allows_tool(&eval_tool)); // Explicitly denied tool
    assert!(!role.allows_tool(&pty_tool)); // Denied category: terminal
    assert!(!role.allows_tool(&sec_tool)); // Denied category: credentials

    assert!(role.check_permission(&nav_tool).is_ok());
    assert!(role.check_permission(&branch_tool).is_ok());
    assert!(role.check_permission(&checkpoint_tool).is_ok());
    assert!(role.check_permission(&eval_tool).is_err());
    assert!(role.check_permission(&pty_tool).is_err());
}

#[test]
fn test_builtin_browser_role() {
    let role = RoleProfile::browser();
    assert_eq!(role.name, "browser");

    let nav_tool = make_tool("browser_navigate", Category::Browser, Tier::Standard);
    let eval_tool = make_tool("browser_eval", Category::Browser, Tier::Dangerous);
    let click_tool = make_tool("mouse_action", Category::Input, Tier::Standard);
    let fs_tool = make_tool("fs_read", Category::Filesystem, Tier::Read);
    let pty_tool = make_tool("pty_spawn", Category::Terminal, Tier::Dangerous);

    assert!(role.allows_tool(&nav_tool));
    // The role narrows only; whether a dangerous tool runs is still the
    // policy's call (access/enable), not the role's.
    assert!(role.allows_tool(&eval_tool));
    assert!(!role.allows_tool(&click_tool));
    assert!(!role.allows_tool(&fs_tool));
    assert!(!role.allows_tool(&pty_tool));

    let resolved = mcp_policy::resolve_role_profile("browser", &Default::default());
    assert_eq!(resolved, Some(role));
}

#[test]
fn test_builtin_operator_role_requires_consent() {
    let role = RoleProfile::operator();
    let dangerous_tool = make_tool("format_disk", Category::System, Tier::Dangerous);
    let standard_tool = make_tool("sys_info", Category::System, Tier::Standard);

    assert!(role.allows_tool(&dangerous_tool));
    assert!(role.requires_consent(&dangerous_tool));
    assert!(!role.requires_consent(&standard_tool));
}

#[test]
fn test_hard_invariants_path_protection_and_traversal() {
    let inv = HardInvariants::default();

    // Standard protected paths
    assert!(is_path_protected("/etc/shadow", &inv.protected_paths));
    assert!(is_path_protected("/etc/sudoers", &inv.protected_paths));
    assert!(is_path_protected(
        "/private/etc/master.passwd",
        &inv.protected_paths
    ));

    // Path traversal bypass attempts
    assert!(is_path_protected(
        "/tmp/../etc/shadow",
        &inv.protected_paths
    ));
    assert!(is_path_protected(
        "/var/log/../../etc/sudoers",
        &inv.protected_paths
    ));
    assert!(is_path_protected(
        "/System/Library/CoreServices",
        &inv.protected_paths
    ));

    // Benign paths
    assert!(!is_path_protected("/tmp/test.txt", &inv.protected_paths));
    assert!(!is_path_protected(
        "/Users/alice/projects/app.rs",
        &inv.protected_paths
    ));

    // Checking JSON arguments
    let evil_args = json!({
        "cmd": "cat",
        "path": "/tmp/../etc/shadow"
    });
    assert!(check_arguments_invariants(&evil_args, &inv).is_err());

    let benign_args = json!({
        "cmd": "echo",
        "path": "/tmp/output.log"
    });
    assert!(check_arguments_invariants(&benign_args, &inv).is_ok());
}

#[test]
fn test_hard_invariants_domain_denial() {
    let mut inv = HardInvariants::default();
    inv.denied_domains.push("malicious.internal".to_string());
    inv.denied_domains.push("phishing.site".to_string());

    assert!(is_domain_denied(
        "https://malicious.internal/login",
        &inv.denied_domains
    ));
    assert!(is_domain_denied(
        "http://api.malicious.internal/keys",
        &inv.denied_domains
    ));
    assert!(is_domain_denied("phishing.site", &inv.denied_domains));
    assert!(!is_domain_denied(
        "https://records.example.com",
        &inv.denied_domains
    ));

    let evil_url_args = json!({
        "url": "https://malicious.internal/steal"
    });
    assert!(check_arguments_invariants(&evil_url_args, &inv).is_err());
}

#[test]
fn test_policy_load_with_custom_role_and_invariants_toml() {
    let toml = r#"
[policy]
categories = ["browser", "system"]
default_role = "app_tester"

[invariants]
protected_paths = ["/sensitive/corp/keys", "/etc/shadow"]
denied_domains = ["exfiltrate.io"]

[roles.app_tester]
description = "Custom testing role"
allowed_categories = ["browser"]
allowed_tiers = ["read", "standard"]
denied_tools = ["browser_eval"]
require_consent_tiers = ["standard"]
"#;

    let cfg = PolicyConfig::from_toml_str(toml).expect("toml parses");
    assert_eq!(cfg.active_role.as_deref(), Some("app_tester"));
    assert_eq!(cfg.invariants.protected_paths.len(), 2);
    assert_eq!(cfg.invariants.denied_domains.len(), 1);

    let role = cfg.get_role("app_tester").expect("custom role found");
    assert_eq!(role.name, "app_tester");

    let read_tool = make_tool("browser_snapshot", Category::Browser, Tier::Read);
    let nav_tool = make_tool("browser_navigate", Category::Browser, Tier::Standard);
    let eval_tool = make_tool("browser_eval", Category::Browser, Tier::Dangerous);
    let sys_tool = make_tool("sys_info", Category::System, Tier::Read);

    assert!(role.allows_tool(&read_tool));
    assert!(role.allows_tool(&nav_tool));
    assert!(!role.allows_tool(&eval_tool)); // Denied tier & denied tool
    assert!(!role.allows_tool(&sys_tool)); // Not in allowed_categories

    assert!(role.requires_consent(&nav_tool)); // Standard tier requires consent in this role
    assert!(!role.requires_consent(&read_tool));
}

#[test]
fn test_policy_gate_enforces_role_and_bypass_cannot_defeat_hard_invariants() {
    let toml = r#"
[policy]
categories = ["browser", "system"]
default_role = "qa"
access = "bypass"

[invariants]
protected_paths = ["/etc/shadow"]
denied_domains = ["evil.org"]
"#;

    let cfg = PolicyConfig::from_toml_str(toml).expect("parses");
    let sink = AuditSink::memory();
    let redactor = mcp_policy::Redactor::empty();
    let policy = Policy::new(cfg, sink, redactor);

    // 1. Role QA denies terminal/eval even when access = "bypass"
    let eval_tool = make_tool("browser_eval", Category::Browser, Tier::Dangerous);
    let decision = policy.gate(&eval_tool);
    match decision {
        Decision::Deny { reason, .. } => {
            assert!(reason.contains("explicitly denied by role 'qa'"));
        }
        _ => panic!("browser_eval must be denied for QA role"),
    }

    // 2. Hard invariant checks reject protected path even if access = "bypass"
    let args = json!({"file": "/tmp/../etc/shadow"});
    let res = policy.check_invariants("read_file", &args);
    assert!(res.is_err());
    assert!(res.unwrap_err().contains("/etc/shadow"));

    // 3. Domain invariant rejects denied domain
    let domain_args = json!({"url": "https://evil.org/exfil"});
    let res = policy.check_invariants("browser_navigate", &domain_args);
    assert!(res.is_err());
    assert!(res.unwrap_err().contains("evil.org"));
}

#[test]
fn test_audit_role_provenance_and_tamper_evidence() {
    let sink = AuditSink::memory().with_role("auditor");
    let mut pre = AuditRecord::pre("sess_rbac", "sys_logs");
    pre.role = Some("auditor".to_string());
    pre.tier = Some("read".to_string());
    sink.write(&pre);

    let mut post = AuditRecord::post("sess_rbac", "sys_logs");
    post.role = Some("auditor".to_string());
    post.ok = Some(true);
    sink.write(&post);

    let raw_records = sink.memory_records();
    let jsonl = raw_records
        .iter()
        .map(|r| r.to_string())
        .collect::<Vec<_>>()
        .join("\n");

    let records = parse_audit_records(&jsonl).expect("parses");
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].role.as_deref(), Some("auditor"));
    assert_eq!(records[1].role.as_deref(), Some("auditor"));

    // 1. Verification succeeds untouched
    let report = verify_audit_log(&jsonl).expect("verifies");
    assert!(report.valid);
    assert_eq!(report.role.as_deref(), Some("auditor"));

    // 2. Compliance exports include role
    let csv = ComplianceExporter::to_hipaa_csv(&records);
    assert!(csv.contains("Role"));
    assert!(csv.contains("auditor"));

    let md = ComplianceExporter::to_soc2_report(&report, &records);
    assert!(md.contains("auditor"));

    let soc2_json = ComplianceExporter::to_soc2_json(&report, &records);
    assert_eq!(soc2_json["role"], "auditor");

    // 3. Tampering with the role invalidates cryptographic ledger
    let mut tampered = raw_records;
    tampered[0]["role"] = json!("admin");
    let tampered_jsonl = tampered
        .iter()
        .map(|r| r.to_string())
        .collect::<Vec<_>>()
        .join("\n");

    let tampered_report = verify_audit_log(&tampered_jsonl).expect("parses");
    assert!(!tampered_report.valid);
    let err = tampered_report.error.expect("error reported");
    assert_eq!(err.seq, 0);
    assert!(err.reason.contains("tampered content hash"));
}

fn denying(domains: &[&str]) -> HardInvariants {
    HardInvariants {
        denied_domains: domains.iter().map(|d| d.to_string()).collect(),
        ..HardInvariants::default()
    }
}

/// Spellings that used to pass the prefix checks and must now be refused.
#[test]
fn invariants_see_through_common_respellings() {
    let inv = denying(&["evil.example"]);
    let refused = [
        // A path inside a command line, not at the start of the string.
        json!({ "data": "cat /etc/shadow\n" }),
        json!({ "cmd": "sh", "args": ["-c", "cp /etc/sudoers /tmp/x"] }),
        // A file: URL.
        json!({ "url": "file:///etc/shadow" }),
        json!({ "url": "FILE://localhost/etc/sudoers" }),
        // Scheme case, userinfo, port, trailing dot.
        json!({ "url": "HTTPS://evil.example/x" }),
        json!({ "url": "https://trusted.example@evil.example/" }),
        json!({ "url": "https://api.evil.example:8443/" }),
        json!({ "url": "https://evil.example./" }),
        json!({ "url": "wss://evil.example/socket" }),
        // A bare host under a host-shaped key.
        json!({ "host": "evil.example" }),
        json!({ "hostname": "cdn.EVIL.example" }),
        // Nested at depth.
        json!({ "steps": [{ "op": "goto", "url": "https://evil.example" }] }),
    ];
    for args in refused {
        assert!(
            check_arguments_invariants(&args, &inv).is_err(),
            "must refuse {args}"
        );
    }
    let allowed = [
        json!({ "url": "https://evil.example.org/" }),
        json!({ "url": "https://notevil.example/" }),
        json!({ "text": "the evil.example domain is blocked" }),
        json!({ "data": "cat /tmp/notes.txt" }),
    ];
    for args in allowed {
        assert!(
            check_arguments_invariants(&args, &inv).is_ok(),
            "must allow {args}"
        );
    }
}

/// On macOS `/etc` is a symlink to `/private/etc`; either spelling of a
/// protected file is the same file.
#[cfg(target_os = "macos")]
#[test]
fn invariants_resolve_symlinked_system_paths() {
    let inv = HardInvariants::default();
    assert!(is_path_protected(
        "/private/etc/sudoers",
        &inv.protected_paths
    ));
    assert!(is_path_protected(
        "/private/etc/pam.d/sudo",
        &inv.protected_paths
    ));
}

/// A symlink an agent creates to a protected file is followed.
#[cfg(unix)]
#[test]
fn invariants_follow_a_link_to_a_protected_path() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("secret.txt");
    std::fs::write(&target, "x").unwrap();
    let link = dir.path().join("innocent.txt");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let protected = vec![target.clone()];
    assert!(is_path_protected(link.to_str().unwrap(), &protected));
}

/// What the invariants cannot see, kept here so nobody mistakes them for
/// containment. They read argument text; they do not run the shell, know
/// another process's working directory, or resolve DNS. The jail, the command
/// allowlist and the SSRF guard are the real limits.
#[test]
fn documented_known_bypasses() {
    let inv = denying(&["evil.example"]);
    let known_bypasses = [
        // Relative to a working directory the policy does not know.
        json!({ "data": "cat ../../../../etc/shadow" }),
        // Shell expansion.
        json!({ "data": "cat /etc/sha*ow" }),
        json!({ "data": "cat /e''tc/shadow" }),
        json!({ "data": "cat $(printf '/etc/%s' shadow)" }),
        // Encoded.
        json!({ "url": "https://evil%2Eexample/" }),
        // An address that resolves to the denied host, or a host typed as
        // free text.
        json!({ "url": "https://203.0.113.7/" }),
        json!({ "text": "curl evil.example" }),
    ];
    for args in known_bypasses {
        assert!(
            check_arguments_invariants(&args, &inv).is_ok(),
            "this bypass is now caught — good; move it out of the \
             known-bypass list and into a positive test: {args}"
        );
    }
}

/// A role tightens the operator's denial budget; it cannot raise or disable it.
#[test]
fn role_cannot_loosen_the_denial_budget() {
    use mcp_policy::Redactor;
    let budget_after = |operator: usize, role_max: usize| {
        let mut cfg = PolicyConfig::from_toml_str(&format!(
            "[policy]\nmax_denials = {operator}\nrole = \"lenient\"\n[roles.lenient]\nmax_denials = {role_max}\n"
        ))
        .expect("config");
        cfg.categories.clear();
        let policy = Policy::new(cfg, AuditSink::memory(), Redactor::empty());
        let mut n = 0;
        while !policy.record_denial() {
            n += 1;
            if n > 100 {
                return usize::MAX;
            }
        }
        n + 1
    };
    assert_eq!(budget_after(5, 50), 5, "role cannot raise it");
    assert_eq!(budget_after(5, 0), 5, "role cannot disable it");
    assert_eq!(budget_after(5, 2), 2, "role can lower it");
    assert_eq!(budget_after(0, 3), 3, "role can impose one");
}
