//! `config.example.toml` is the file operators copy. It has to stay both
//! *parseable* and *safe to copy verbatim*.
//!
//! Documentation drifts silently — a key gets renamed, a section is added and
//! the example is not updated, or an example gets loosened during debugging and
//! shipped that way. The first failure mode wastes an afternoon; the second
//! hands every operator who copied the file a wider policy than they meant to
//! have. Both are cheap to pin here.

use std::path::PathBuf;

use mcp_policy::{Mode, PolicyConfig};

fn example_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config.example.toml")
}

fn example() -> String {
    std::fs::read_to_string(example_path())
        .expect("config.example.toml must exist at the repo root")
}

/// The parser tolerates unknown keys for forward-compatibility, so a typo would
/// not fail here — but a wrong *type* or a bad value is a hard error, and that
/// is the class of mistake this catches.
#[test]
fn the_example_config_parses() {
    PolicyConfig::from_toml_str(&example()).expect("config.example.toml must parse");
}

/// Every section header in the example must be one the loader actually reads.
/// A section nobody consumes is documentation for a feature that does not
/// exist, which is worse than no documentation.
#[test]
fn every_documented_section_is_a_real_one() {
    const KNOWN: &[&str] = &[
        "policy",
        "input",
        "fs",
        "terminal",
        "network",
        "credentials",
        "packages",
        "memory",
        "browser",
        "vision",
        "judge",
        "http",
    ];
    for line in example().lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            assert!(
                KNOWN.contains(&name),
                "[{name}] is documented but not read by the loader"
            );
        }
    }
}

/// The example must be safe to copy without editing. Each assertion below is a
/// default that, if flipped, hands the agent something it should have had to
/// ask for.
#[test]
fn the_example_is_closed_by_default() {
    let cfg = PolicyConfig::from_toml_str(&example()).unwrap();

    assert!(
        !cfg.allow_shell,
        "terminal.allow_shell must stay off: a shell defeats the command allowlist"
    );
    assert!(
        !cfg.allow_private_network,
        "network.allow_private must stay off: it reopens loopback and cloud metadata"
    );
    assert!(
        !cfg.browser_allow_private,
        "browser.allow_private must stay off: the browser is a network client inside the perimeter"
    );
    assert!(
        !cfg.judge.enabled,
        "judge.enabled must stay off: it sends UI text to a remote model"
    );
    assert!(
        !cfg.allow_arbitrary_source,
        "packages.allow_arbitrary_source must stay off: it is malware delivery by another name"
    );
    assert!(
        !cfg.http_enabled,
        "http.enabled must stay off: stdio needs no listening socket"
    );
    assert!(
        cfg.http_allowed_origins.is_empty(),
        "http.allowed_origins must stay empty: any web page can reach 127.0.0.1"
    );
    assert!(
        cfg.enable.is_empty(),
        "policy.enable must stay empty: no dangerous tool is opted in by default"
    );
    assert!(
        cfg.allowed_commands.is_empty(),
        "terminal.allowed_commands must stay empty: the exec engine runs nothing by default"
    );
    assert!(
        cfg.allowed_hosts.is_empty(),
        "network.allowed_hosts must stay empty: the net engine reaches nothing by default"
    );
    assert!(
        cfg.allowed_services.is_empty(),
        "credentials.allowed_services must stay empty"
    );
    assert!(
        cfg.allowed_sources.is_empty(),
        "packages.allowed_sources must stay empty"
    );
    assert!(
        cfg.fs_roots.is_empty(),
        "fs.roots must stay empty: an empty jail refuses everything, which is the safe end"
    );
    assert_eq!(
        cfg.mode,
        Mode::Interactive,
        "policy.mode must stay interactive: autonomous turns every consent prompt into a denial, \
         which is safe, but shipping it as the example hides the consent UX entirely"
    );
}

/// The documented capture tunables must match the built-in defaults, so the
/// example describes what an operator actually gets rather than something
/// slightly different.
#[test]
fn documented_vision_values_match_the_defaults() {
    let cfg = PolicyConfig::from_toml_str(&example()).unwrap();
    let defaults = PolicyConfig::default();
    assert_eq!(cfg.vision_detail_low_px, defaults.vision_detail_low_px);
    assert_eq!(
        cfg.vision_detail_balanced_px,
        defaults.vision_detail_balanced_px
    );
    assert_eq!(cfg.vision_detail_full_px, defaults.vision_detail_full_px);
    assert_eq!(cfg.vision_default_detail, defaults.vision_default_detail);
    assert_eq!(cfg.vision_unchanged_mad, defaults.vision_unchanged_mad);
    assert_eq!(
        cfg.vision_pixels_per_token,
        defaults.vision_pixels_per_token
    );
    assert_eq!(cfg.vision_max_image_bytes, defaults.vision_max_image_bytes);
}
