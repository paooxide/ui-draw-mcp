//! `agentctl audit keygen | verify --pubkey` through the real binary.

use std::path::Path;
use std::process::{Command, Output};

fn agentctl(cfg: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentctl"))
        .env("AGENTCTL_CONFIG", cfg)
        .args(args)
        .output()
        .expect("run agentctl")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn keygen_then_verify_pins_the_signer() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.toml");
    std::fs::write(&cfg, "").unwrap();
    let key = dir.path().join("audit.key");

    let out = agentctl(&cfg, &["audit", "keygen", "--out", key.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let pubkey = stdout(&out)
        .lines()
        .find_map(|l| l.strip_prefix("public key:"))
        .map(|s| s.trim().to_string())
        .expect("keygen prints the public key");

    // A log written the way `serve` writes it with the key configured.
    let signing = mcp_policy::load_signing_key(&key).unwrap();
    let sink =
        mcp_policy::AuditSink::file_with_key(dir.path().to_path_buf(), "sess", Some(signing))
            .unwrap();
    sink.write(&mcp_policy::AuditRecord::pre("sess", "ping"));
    let log = sink.path().unwrap();
    let log = log.to_str().unwrap();

    let pinned = agentctl(&cfg, &["audit", "verify", log, "--pubkey", &pubkey]);
    assert!(pinned.status.success());
    assert!(stdout(&pinned).contains("signed by the pinned key"));

    // The value of --pubkey must not be taken for the file argument.
    let flag_first = agentctl(&cfg, &["audit", "verify", "--pubkey", &pubkey, log]);
    assert!(flag_first.status.success(), "{}", stdout(&flag_first));

    let unpinned = agentctl(&cfg, &["audit", "verify", log]);
    assert!(unpinned.status.success());
    assert!(stdout(&unpinned).contains("SELF-CONSISTENT"));
    assert!(!stdout(&unpinned).contains("VERIFIED"));

    let other = dir.path().join("other.key");
    let out = agentctl(&cfg, &["audit", "keygen", "--out", other.to_str().unwrap()]);
    let wrong = stdout(&out)
        .lines()
        .find_map(|l| l.strip_prefix("public key:"))
        .unwrap()
        .trim()
        .to_string();
    let mismatch = agentctl(&cfg, &["audit", "verify", log, "--pubkey", &wrong]);
    assert_eq!(mismatch.status.code(), Some(1));
}
