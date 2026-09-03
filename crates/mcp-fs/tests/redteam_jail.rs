//! Red-team suite for filesystem containment, against real files and real
//! symlinks on the real filesystem — the escapes that matter are properties of
//! the OS's path resolution, and a stubbed filesystem would not have them.
//!
//! The rule under test: **resolve fully, then check**. Every case here is an
//! attempt to make the resolved path differ from what the string looks like.

use std::path::{Path, PathBuf};

use mcp_fs::{default_denied, Jail, PathError};

/// A fresh, canonicalised sandbox per test. `/tmp` is itself a symlink to
/// `/private/tmp` on macOS, so canonicalising is what keeps the root and the
/// resolved paths comparable.
fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("mcp-rt-jail-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("inside")).unwrap();
    std::fs::write(d.join("inside/ok.txt"), b"hi").unwrap();
    std::fs::canonicalize(&d).unwrap()
}

fn jail_at(root: &Path) -> Jail {
    Jail::new(vec![root.to_path_buf()], default_denied())
}

fn s(p: &std::path::Path) -> String {
    p.to_str().unwrap().to_string()
}

/// Credential stores are denied by substring. macOS ships a **case-insensitive**
/// filesystem by default, so `~/.SSH/id_rsa` opens exactly the same file as
/// `~/.ssh/id_rsa` — a lowercase-only comparison is a bypass, not a nicety.
#[test]
fn case_variant_credential_paths_are_still_denied() {
    let root = sandbox("case");
    std::fs::create_dir_all(root.join(".SSH")).unwrap();
    std::fs::write(root.join(".SSH/id_rsa"), b"key").unwrap();
    std::fs::create_dir_all(root.join(".AWS")).unwrap();
    std::fs::write(root.join(".AWS/credentials"), b"key").unwrap();
    let j = jail_at(&root);
    for p in [
        ".SSH/id_rsa",
        ".Ssh/id_rsa",
        ".AWS/credentials",
        ".aws/credentials",
    ] {
        let full = s(&root.join(p));
        assert!(
            matches!(j.resolve(&full), Err(PathError::Denied(_))),
            "case-variant credential path must be denied: {p}"
        );
    }
}

/// The plain form must still be denied — the case fix must not have replaced
/// the original comparison with a broken one.
#[test]
fn credential_stores_are_denied() {
    let root = sandbox("creds");
    for dir in [".ssh", ".aws", ".gnupg", ".kube"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    std::fs::write(root.join(".netrc"), b"x").unwrap();
    let j = jail_at(&root);
    for p in [
        ".ssh/id_rsa",
        ".aws/credentials",
        ".gnupg/secring.gpg",
        ".kube/config",
        ".netrc",
    ] {
        let full = s(&root.join(p));
        assert!(
            matches!(j.resolve(&full), Err(PathError::Denied(_))),
            "must be denied: {p}"
        );
        // ...and the directory itself, not only paths under it.
    }
    assert!(matches!(
        j.resolve(&s(&root.join(".ssh"))),
        Err(PathError::Denied(_))
    ));
}

/// A symlink inside the root pointing out is the classic escape. Resolution
/// must follow it and then judge where it actually landed.
#[test]
fn symlink_pointing_out_of_the_root_is_refused() {
    let root = sandbox("symlink");
    std::os::unix::fs::symlink("/etc", root.join("escape")).unwrap();
    let j = jail_at(&root);
    assert!(matches!(
        j.resolve(&s(&root.join("escape/passwd"))),
        Err(PathError::Escapes(_))
    ));
    assert!(matches!(
        j.resolve(&s(&root.join("escape"))),
        Err(PathError::Escapes(_))
    ));
}

/// The subtler version: the file being *created* does not exist, so there is
/// nothing to canonicalise — but its parent is a symlink pointing out. This is
/// why resolution walks up to the nearest existing ancestor.
#[test]
fn symlinked_parent_cannot_be_used_to_create_outside() {
    let root = sandbox("symparent");
    let outside = std::env::temp_dir().join(format!("mcp-rt-outside-{}", std::process::id()));
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("out")).unwrap();
    let j = jail_at(&root);
    assert!(
        matches!(
            j.resolve(&s(&root.join("out/newfile.txt"))),
            Err(PathError::Escapes(_))
        ),
        "writing through a symlinked parent must be refused"
    );
    let _ = std::fs::remove_dir_all(&outside);
}

/// A symlink that stays inside is legitimate and must keep working, or the
/// jail is unusable in any real project directory.
#[test]
fn symlink_staying_inside_the_root_is_allowed() {
    let root = sandbox("syminside");
    std::os::unix::fs::symlink(root.join("inside"), root.join("alias")).unwrap();
    let j = jail_at(&root);
    let got = j.resolve(&s(&root.join("alias/ok.txt"))).unwrap();
    assert_eq!(got, root.join("inside/ok.txt"));
}

/// Prefix containment must be by path component, not by string: `/tmp/jail` and
/// `/tmp/jail-evil` share a textual prefix but are unrelated directories.
#[test]
fn sibling_directory_sharing_a_textual_prefix_is_outside() {
    let root = sandbox("prefix");
    let sibling = PathBuf::from(format!("{}-evil", root.display()));
    std::fs::create_dir_all(&sibling).unwrap();
    std::fs::write(sibling.join("loot.txt"), b"x").unwrap();
    let j = jail_at(&root);
    assert!(matches!(
        j.resolve(&s(&sibling.join("loot.txt"))),
        Err(PathError::Escapes(_))
    ));
    let _ = std::fs::remove_dir_all(&sibling);
}

/// `..` must lose, both in the resolved part of the path and in the tail that
/// does not exist yet.
#[test]
fn traversal_is_refused_resolved_or_not() {
    let root = sandbox("traverse");
    let j = jail_at(&root);
    for p in [
        "inside/../../../../etc/passwd",
        "inside/ok.txt/../../../../etc/passwd",
        "does_not_exist/../../../../etc/passwd",
        "inside/../..",
    ] {
        let full = s(&root.join(p));
        assert!(j.resolve(&full).is_err(), "traversal must be refused: {p}");
    }
}

/// Absolute paths outside every root are refused even when they plainly exist.
#[test]
fn absolute_paths_outside_the_roots_are_refused() {
    let root = sandbox("abs");
    let j = jail_at(&root);
    for p in ["/etc/passwd", "/etc/hosts", "/", "/var", "/System"] {
        assert!(
            matches!(j.resolve(p), Err(PathError::Escapes(_))),
            "must be refused: {p}"
        );
    }
}

/// `~` is never expanded: doing so would silently widen scope to the whole home
/// directory regardless of what the roots say.
#[test]
fn tilde_is_not_expanded() {
    let root = sandbox("tilde");
    let j = jail_at(&root);
    for p in ["~", "~/", "~/.ssh/id_rsa", "~root/.ssh/id_rsa"] {
        assert!(
            matches!(j.resolve(p), Err(PathError::Invalid(_))),
            "tilde must not be expanded: {p}"
        );
    }
}

/// Degenerate inputs fail closed rather than resolving to the root or the cwd.
#[test]
fn degenerate_inputs_are_refused() {
    let root = sandbox("degenerate");
    let j = jail_at(&root);
    assert!(matches!(j.resolve(""), Err(PathError::Invalid(_))));
    assert!(j.resolve("\0").is_err());
    assert!(j.resolve("inside/ok\0.txt").is_err());
}

/// Relative paths bind to the first root. Inheriting the process cwd would hand
/// the agent whatever directory the operator happened to launch from.
#[test]
fn relative_paths_bind_to_the_root_not_the_cwd() {
    let root = sandbox("relative");
    let j = jail_at(&root);
    let got = j.resolve("inside/ok.txt").unwrap();
    assert_eq!(got, root.join("inside/ok.txt"));
    // This file exists in the process cwd but not in the root; it must resolve
    // to the root's (absent) copy rather than the one the operator launched from.
    assert_eq!(j.resolve("Cargo.toml").unwrap(), root.join("Cargo.toml"));
    assert!(std::path::Path::new("Cargo.toml").exists());
}

/// No roots configured means the engine refuses everything — it does not
/// default to the whole disk.
#[test]
fn no_roots_means_nothing_resolves() {
    let j = Jail::new(vec![], default_denied());
    assert!(j.is_empty());
    for p in ["/etc/passwd", "relative.txt", "/tmp"] {
        assert!(j.resolve(p).is_err(), "must be refused with no roots: {p}");
    }
}

/// Denied patterns are applied to the *resolved* path, so a symlink cannot be
/// used to launder a credential store into an innocent-looking name.
#[test]
fn denied_patterns_apply_after_symlink_resolution() {
    let root = sandbox("laundry");
    std::fs::create_dir_all(root.join(".ssh")).unwrap();
    std::fs::write(root.join(".ssh/id_rsa"), b"key").unwrap();
    std::os::unix::fs::symlink(root.join(".ssh"), root.join("innocent")).unwrap();
    let j = jail_at(&root);
    assert!(
        matches!(
            j.resolve(&s(&root.join("innocent/id_rsa"))),
            Err(PathError::Denied(_))
        ),
        "a symlink must not launder a denied path"
    );
}

/// Multiple roots are each honoured, and containment is per-root rather than
/// "inside the union's common prefix".
#[test]
fn multiple_roots_are_independent() {
    let a = sandbox("multi-a");
    let b = sandbox("multi-b");
    let j = Jail::new(vec![a.clone(), b.clone()], default_denied());
    assert!(j.resolve(&s(&a.join("inside/ok.txt"))).is_ok());
    assert!(j.resolve(&s(&b.join("inside/ok.txt"))).is_ok());
    assert!(j.resolve("/etc/passwd").is_err());
}

/// The honest half of the contract.
///
/// A **hard link** inside a root that points at an inode outside it is
/// invisible to path resolution: a hard link has no target path to follow, only
/// a shared inode. `realpath` reports the in-root path, so the jail allows it.
///
/// This is not exploitable by the agent on its own — the filesystem engine
/// exposes no hard-link primitive (`fs_symlink` is the only link tool, and it
/// jails both endpoints) — so creating one requires an actor that already has
/// out-of-band write access inside a root. Recorded here so the limit is a
/// known property rather than a surprise.
#[test]
fn documented_known_gap_hard_links() {
    let root = sandbox("hardlink");
    let outside = std::env::temp_dir().join(format!("mcp-rt-hl-{}.txt", std::process::id()));
    std::fs::write(&outside, b"secret").unwrap();
    let linked = root.join("looks_local.txt");
    // Same filesystem, so this succeeds; across volumes it would not.
    if std::fs::hard_link(&outside, &linked).is_ok() {
        let j = jail_at(&root);
        let resolved = j
            .resolve(&s(&linked))
            .expect("hard links resolve as in-root");
        assert!(
            resolved.starts_with(&root),
            "documenting that a hard link reads as inside the root"
        );
    }
    let _ = std::fs::remove_file(&outside);
}

/// The server's own state directory is denied even inside a root.
///
/// With a root of `~` — the natural thing for an operator to configure — every
/// control the jail protects would otherwise be editable through the jail
/// itself: `config.toml` decides which categories are enabled, the audit log is
/// the record of what the agent did, and `STOP` is the kill switch. Rewriting
/// the first, truncating the second and deleting the third are the three moves
/// that turn a contained agent into an uncontained one.
///
/// This also denies *reading* the kill-switch file through `fs_read`, which is
/// intended: the agent has no business inspecting its own leash.
#[test]
fn the_agentctl_state_directory_is_denied_inside_a_root() {
    let root = sandbox("agentctl-state");
    std::fs::create_dir_all(root.join(".agentctl/audit")).unwrap();
    std::fs::write(root.join(".agentctl/config.toml"), b"[policy]").unwrap();
    std::fs::write(root.join(".agentctl/STOP"), b"").unwrap();
    std::fs::write(root.join(".agentctl/audit/s1.jsonl"), b"{}").unwrap();
    let j = jail_at(&root);
    for p in [
        ".agentctl/config.toml",
        ".agentctl/STOP",
        ".agentctl/audit/s1.jsonl",
        // Case-insensitive, like every other deny-list entry.
        ".AgentCtl/config.toml",
    ] {
        assert!(
            matches!(j.resolve(&s(&root.join(p))), Err(PathError::Denied(_))),
            "{p} must be denied inside a root"
        );
    }
    // A sibling that merely starts with the same letters is not the state dir.
    std::fs::write(root.join("agentctl-notes.md"), b"notes").unwrap();
    assert!(j.resolve(&s(&root.join("agentctl-notes.md"))).is_ok());
}
