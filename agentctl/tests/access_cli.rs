//! `--access`, `AGENTCTL_ACCESS`, `--dry-run`, `--role` and the config notes,
//! through the real binary. The pure rules are table-tested next to the code
//! (`resolve_access`, `parse_cli`); this proves they are wired to the process.

use std::path::Path;
use std::process::{Command, Output};

fn run(config: &str, env: &[(&str, &str)], args: &[&str]) -> (Output, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.toml");
    std::fs::write(&cfg, config).unwrap();
    // Keep the audit and STOP paths out of the real home.
    let out = command(&cfg, env)
        .args(args)
        .output()
        .expect("run agentctl");
    (out, dir)
}

fn command(cfg: &Path, env: &[(&str, &str)]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_agentctl"));
    c.env("AGENTCTL_CONFIG", cfg);
    for var in [
        "AGENTCTL_ACCESS",
        "AGENTCTL_ROLE",
        "AGENTCTL_FOREGROUND",
        "AGENTCTL_DEMO",
        "AGENTCTL_DEMO_SPEED",
        "AGENTCTL_ANONYMIZE",
    ] {
        c.env_remove(var);
    }
    for (k, v) in env {
        c.env(k, v);
    }
    c
}

fn text(o: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}

/// The level and its source, as `doctor` prints them.
fn doctor_access(config: &str, env: &[(&str, &str)], args: &[&str]) -> String {
    let mut argv = vec!["doctor"];
    argv.extend_from_slice(args);
    let (out, _dir) = run(config, env, &argv);
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stderr}");
    stdout
        .lines()
        .find(|l| l.trim_start().starts_with("access:"))
        .unwrap_or_else(|| panic!("no access line in:\n{stdout}"))
        .trim()
        .to_string()
}

fn refused(config: &str, env: &[(&str, &str)], args: &[&str]) -> String {
    let (out, _dir) = run(config, env, args);
    let (_, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("refusing to start") || stderr.contains("agentctl:"),
        "{stderr}"
    );
    stderr
}

#[test]
fn a_flag_or_environment_level_is_reported_with_where_it_came_from() {
    let line = doctor_access("", &[], &["--access", "ask"]);
    assert!(line.contains("ask (--access flag)"), "{line}");
    let line = doctor_access("", &[("AGENTCTL_ACCESS", "auto")], &[]);
    assert!(line.contains("auto (AGENTCTL_ACCESS)"), "{line}");
    let line = doctor_access("[policy]\naccess = \"auto\"\n", &[], &[]);
    assert!(line.contains("auto (config file)"), "{line}");
    let line = doctor_access("", &[], &[]);
    assert!(line.contains("not set (default)"), "{line}");
    // The flag stands in for the environment when both are given.
    let line = doctor_access("", &[("AGENTCTL_ACCESS", "auto")], &["--access", "ask"]);
    assert!(line.contains("ask (--access flag)"), "{line}");
    // `--access bypass` is explicit, so it is honoured with no file level.
    let line = doctor_access("", &[], &["--access", "bypass"]);
    assert!(line.contains("bypass (--access flag)"), "{line}");
}

#[test]
fn a_flag_or_environment_level_cannot_loosen_the_file_and_says_so() {
    let e = refused(
        "[policy]\naccess = \"ask\"\n",
        &[],
        &["doctor", "--access", "auto"],
    );
    assert!(e.contains("would loosen") && e.contains("--access"), "{e}");
    let e = refused(
        "[policy]\naccess = \"auto\"\n",
        &[("AGENTCTL_ACCESS", "bypass")],
        &["doctor"],
    );
    assert!(
        e.contains("would loosen") && e.contains("AGENTCTL_ACCESS"),
        "{e}"
    );
    // Tightening is fine, and shows as such.
    let line = doctor_access(
        "[policy]\naccess = \"bypass\"\n",
        &[("AGENTCTL_ACCESS", "ask")],
        &[],
    );
    assert!(line.contains("ask (AGENTCTL_ACCESS)"), "{line}");
}

#[test]
fn bypass_is_never_taken_from_the_environment_alone() {
    let e = refused("", &[("AGENTCTL_ACCESS", "bypass")], &["doctor"]);
    assert!(
        e.contains("not honoured on its own") && e.contains("--access bypass"),
        "{e}"
    );
}

#[test]
fn an_invalid_level_is_a_startup_error_from_either_source() {
    let e = refused("", &[("AGENTCTL_ACCESS", "yolo")], &["doctor"]);
    assert!(e.contains("AGENTCTL_ACCESS") && e.contains("yolo"), "{e}");
    let e = refused("", &[], &["doctor", "--access", "yolo"]);
    assert!(e.contains("--access") && e.contains("yolo"), "{e}");
    let e = refused("", &[], &["doctor", "--access"]);
    assert!(e.contains("--access needs a value"), "{e}");
}

/// `agentctl --role serve` used to run the default command with a role named
/// `serve`; now it is a plain usage error.
#[test]
fn a_flag_value_does_not_swallow_a_command() {
    let e = refused("", &[], &["--role", "serve"]);
    assert!(
        e.contains("--role") && e.contains("serve") && e.contains("--role=serve"),
        "{e}"
    );
    let e = refused("", &[], &["--role"]);
    assert!(e.contains("--role needs a value"), "{e}");
}

#[test]
fn dry_run_shows_in_doctor_for_the_flag_and_both_config_spellings() {
    for (config, args) in [
        ("[policy]\naccess = \"auto\"\ndry_run = true\n", vec![]),
        ("[policy]\naccess = \"ask\"\nmode = \"dry_run\"\n", vec![]),
        ("[policy]\naccess = \"auto\"\n", vec!["--dry-run"]),
        ("", vec!["--dry-run"]),
    ] {
        let mut argv = vec!["doctor"];
        argv.extend(args);
        let (out, _dir) = run(config, &[], &argv);
        let (stdout, stderr) = text(&out);
        assert!(out.status.success(), "{stderr}");
        assert!(
            stdout.contains("dry run:         on"),
            "{config:?}\n{stdout}"
        );
    }
    let (out, _dir) = run("", &[], &["doctor"]);
    assert!(!text(&out).0.contains("dry run:"));
}

#[test]
fn doctor_prints_the_three_ideas_and_the_notes() {
    let (out, _dir) = run(
        "[policy]\naccess = \"ask\"\ncatgories = [\"vision\"]\nmax_denials = 5\nmode = \"autonomous\"\n\
         [input]\ndemo = true\nforeground = \"never\"\n",
        &[],
        &["doctor", "--role", "operator"],
    );
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stderr}");
    for needle in [
        "access:          ask (config file): act freely; dangerous tools and high-impact actions ask a human",
        "scope:           12 categories",
        "stops:           kill switch",
        "foreground never",
        "ignored keys:    policy.catgories (did you mean policy.categories?)",
        "policy.max_denials has no effect and never had",
        "policy.mode is ignored because access = \"ask\"",
        "input.demo is an older spelling of policy.demo",
        "role 'operator' is deprecated",
    ] {
        assert!(stdout.contains(needle), "missing {needle:?} in:\n{stdout}");
    }
    // The same warnings were logged at startup.
    assert!(
        stderr.contains("policy.max_denials has no effect"),
        "{stderr}"
    );
}

#[test]
fn conflicting_spellings_stop_startup() {
    let e = refused(
        "[policy]\ndemo = false\n[input]\ndemo = true\n",
        &[],
        &["doctor"],
    );
    assert!(e.contains("two spellings of one setting"), "{e}");
}

#[test]
fn config_print_shows_the_level() {
    let (out, _dir) = run("[policy]\naccess = \"auto\"\n", &[], &["config", "print"]);
    let (stdout, _) = text(&out);
    assert!(
        stdout.contains("access = \"auto\"") && !stdout.contains("max_denials"),
        "{stdout}"
    );
}
