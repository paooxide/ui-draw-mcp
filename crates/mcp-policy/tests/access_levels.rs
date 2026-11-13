//! The three levels at the policy's own boundary: what a config means after
//! `access`, `categories`, the legacy keys and the role presets are laid
//! together, proven through the real loader and the real gate.

use mcp_policy::{
    deprecated_role_note, resolve_role_profile, Access, AuditSink, Decision, Mode, Policy,
    PolicyConfig, Redactor, RoleProfile,
};
use mcp_types::{Category, ErrorCode, Tier, ToolDescriptor};
use serde_json::{json, Value};

include!("fixtures/legacy_config_cases.rs");

fn tool(name: &str, category: Category, tier: Tier) -> ToolDescriptor {
    ToolDescriptor::new(name, category, tier, "", json!({"type": "object"}))
}

fn policy(config: &str) -> Policy {
    let cfg = PolicyConfig::from_toml_str(config).unwrap_or_else(|e| panic!("{e}\n{config}"));
    Policy::new(cfg, AuditSink::memory(), Redactor::empty())
}

fn denied(d: &Decision) -> bool {
    matches!(
        d,
        Decision::Deny {
            code: ErrorCode::PolicyDenied,
            ..
        }
    )
}

/// A category the file does not list is neither callable nor visible, under
/// every level; one it lists behaves as the level says. Before, the list was
/// discarded and every category was on.
#[test]
fn categories_narrow_every_level_through_the_real_gate() {
    let vision_read = tool("get_ui_tree", Category::Vision, Tier::Read);
    let vision_danger = tool("power_like", Category::Vision, Tier::Dangerous);
    let terminal_std = tool("exec", Category::Terminal, Tier::Standard);
    let terminal_danger = tool("pty_like", Category::Terminal, Tier::Dangerous);
    for level in ["ask", "auto", "bypass"] {
        let p = policy(&format!(
            "[policy]\naccess = \"{level}\"\ncategories = [\"vision\"]\n"
        ));
        assert_eq!(p.gate(&vision_read), Decision::Allow, "{level}");
        assert!(p.allows_tool(&vision_read), "{level}");
        // Not listed: refused, whatever the tier, and not advertised.
        for t in [&terminal_std, &terminal_danger] {
            assert!(denied(&p.gate(t)), "{level} {}", t.name);
            assert!(!p.allows_tool(t), "{level} {}", t.name);
        }
        // Listed: the level's own rule for a dangerous tool.
        match (level, p.gate(&vision_danger)) {
            ("ask", Decision::NeedConsent { .. }) => {}
            ("auto" | "bypass", Decision::Allow) => {}
            (l, d) => panic!("{l}: {d:?}"),
        }
        // No list: every category is on, as before.
        let all = policy(&format!("[policy]\naccess = \"{level}\"\n"));
        assert!(!denied(&all.gate(&terminal_std)), "{level}");
        assert!(all.allows_tool(&terminal_danger), "{level}");
    }
}

/// A level from a flag over a file that lists categories is narrowed by them.
#[test]
fn a_flag_level_keeps_the_files_category_list() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "[policy]\ncategories = [\"vision\"]\n").unwrap();
    let cfg = PolicyConfig::load_from_with(&path, Some(Access::Auto), false).unwrap();
    assert_eq!(cfg.access, Some(Access::Auto));
    assert_eq!(cfg.categories, vec![Category::Vision]);
}

/// The no-`access` path is the one existing configs use. Its effective config,
/// and what the gate does with it, must not move: this compares six
/// representative configs, captured before the three-level change, with what
/// the loader produces now. The only differences allowed are the removed
/// `max_denials` fields.
#[test]
fn legacy_configs_without_access_load_exactly_as_before() {
    let expected: Value =
        serde_json::from_str(include_str!("fixtures/legacy_effective_config.json"))
            .expect("fixture parses");
    let home = mcp_policy::default_agentctl_dir().display().to_string();
    for (name, text) in CASES {
        let cfg = PolicyConfig::from_toml_str(text).unwrap();
        assert_eq!(cfg.access, None, "{name}: a legacy config sets no access");
        let mut roles = std::collections::BTreeMap::new();
        for (k, v) in &cfg.roles {
            roles.insert(k.clone(), serde_json::to_value(v).unwrap());
        }
        let got = json!({
            "effective": cfg.to_redacted_json(),
            "extra": {
                "active_role": cfg.active_role,
                "invariants": serde_json::to_value(&cfg.invariants).unwrap(),
                "roles": roles,
                "role_profile": cfg.active_role_profile().map(|r| serde_json::to_value(r).unwrap()),
            },
        });
        // Paths are under $HOME, which differs by machine.
        let got: Value =
            serde_json::from_str(&got.to_string().replace(&home, "<AGENTCTL_DIR>")).unwrap();

        // The one intended difference: `max_denials` is gone.
        let mut want = expected[name].clone();
        want["effective"]["policy"]
            .as_object_mut()
            .unwrap()
            .remove("max_denials")
            .expect("the fixture predates the removal");
        for role in want["extra"]["roles"].as_object_mut().unwrap().values_mut() {
            role.as_object_mut().unwrap().remove("max_denials");
        }
        if let Some(p) = want["extra"]["role_profile"].as_object_mut() {
            p.remove("max_denials");
        }
        assert_eq!(got, want, "{name}");
    }
}

/// The names that are no longer documented still resolve to the same profile
/// they always did, so nothing breaks and nothing loosens.
#[test]
fn deprecated_role_names_resolve_to_the_same_profiles() {
    let none = Default::default();
    let by = |n: &str| resolve_role_profile(n, &none).unwrap_or_else(|| panic!("{n}"));
    // Aliases resolve to the profile of the name they stand for.
    assert_eq!(by("auditor"), RoleProfile::readonly());
    assert_eq!(by("tester"), RoleProfile::qa());
    assert_eq!(by("AUDITOR"), RoleProfile::readonly());
    assert_eq!(by("superadmin"), RoleProfile::admin());
    // And the profiles themselves are what they were.
    let operator = by("operator");
    assert_eq!(operator.require_consent_for_tiers, vec![Tier::Dangerous]);
    assert!(operator.require_consent_for_tools.is_empty());
    assert!(operator.allowed_tiers.is_none() && operator.allowed_categories.is_none());
    assert!(operator.denied_categories.is_empty() && operator.denied_tools.is_empty());
    let admin = by("admin");
    assert!(
        admin.require_consent_for_tiers.is_empty() && admin.require_consent_for_tools.is_empty()
    );
    assert!(admin.allowed_tiers.is_none() && admin.allowed_categories.is_none());
    assert!(admin.allowed_tools.is_none() && admin.denied_tools.is_empty());
    assert!(admin.denied_categories.is_empty());
    let readonly = by("readonly");
    assert_eq!(readonly.allowed_tiers, Some(vec![Tier::Read]));
    assert!(readonly.denied_tools.contains(&"browser_eval".to_string()));
    for kept in ["readonly", "qa", "browser", "browser-core"] {
        assert!(resolve_role_profile(kept, &none).is_some(), "{kept}");
    }
    assert!(resolve_role_profile("nonesuch", &none).is_none());
}

#[test]
fn a_deprecated_role_name_names_its_replacement_and_a_documented_one_does_not() {
    let none = Default::default();
    for (name, instead) in [
        ("operator", "access = \"ask\""),
        ("admin", "remove the role"),
        ("superadmin", "remove the role"),
        ("auditor", "role = \"readonly\""),
        ("tester", "role = \"qa\""),
        ("Operator", "access = \"ask\""),
    ] {
        let note = deprecated_role_note(name, &none).unwrap_or_else(|| panic!("{name}"));
        assert!(
            note.contains(&format!("'{name}'"))
                && note.contains("deprecated")
                && note.contains(instead),
            "{note}"
        );
    }
    for quiet in [
        "readonly",
        "qa",
        "browser",
        "browser-core",
        "nonesuch",
        "mine",
    ] {
        assert_eq!(deprecated_role_note(quiet, &none), None, "{quiet}");
    }
    // A role the operator defined under one of those names is theirs.
    let cfg = PolicyConfig::from_toml_str(
        "[policy]\nrole = \"admin\"\n[roles.admin]\ndenied_tools = [\"exec\"]\n",
    )
    .unwrap();
    assert_eq!(cfg.deprecated_role_note(), None);
    // Asked of the config, the active role decides.
    let cfg = PolicyConfig::from_toml_str("[policy]\nrole = \"operator\"\n").unwrap();
    assert!(cfg
        .deprecated_role_note()
        .unwrap()
        .contains("access = \"ask\""));
    assert_eq!(PolicyConfig::default().deprecated_role_note(), None);
}

/// A custom role keeps working end to end, through the older spellings of its
/// consent list too, and a `max_denials` in it loads and does nothing.
#[test]
fn custom_roles_keep_working() {
    let p = policy(
        "[policy]\nrole = \"support\"\ncategories = [\"browser\", \"vision\"]\n\
         [roles.support]\nallowed_categories = [\"browser\"]\ndenied_tools = [\"browser_eval\"]\n\
         require_consent_for_tiers = [\"standard\"]\nmax_denials = 1\n",
    );
    let nav = tool("browser_navigate", Category::Browser, Tier::Standard);
    let eval = tool("browser_eval", Category::Browser, Tier::Dangerous);
    let shot = tool("capture_screen", Category::Vision, Tier::Read);
    assert!(matches!(p.gate(&nav), Decision::NeedConsent { .. }));
    assert!(denied(&p.gate(&eval)));
    assert!(denied(&p.gate(&shot)));
    // `max_denials = 1` aborted nothing: further denials are refused the same way.
    for _ in 0..5 {
        assert!(denied(&p.gate(&eval)));
    }
}

/// `Policy::is_dry_run` follows the effective mode, so `dry_run` reaches the
/// server under every level.
#[test]
fn dry_run_reaches_the_policy_under_every_level() {
    for level in ["ask", "auto", "bypass"] {
        for spelling in ["dry_run = true", "mode = \"dry_run\""] {
            let p = policy(&format!("[policy]\naccess = \"{level}\"\n{spelling}\n"));
            assert!(p.is_dry_run(), "{level} {spelling}");
            assert_eq!(p.mode(), Mode::DryRun);
            assert_eq!(p.access().map(|a| a.as_str()), Some(level));
        }
        assert!(!policy(&format!("[policy]\naccess = \"{level}\"\n")).is_dry_run());
    }
}
