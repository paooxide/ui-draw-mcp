//! Path containment — the security core of the filesystem engine.
//!
//! Every path an agent supplies is untrusted. Containment rests on one rule:
//! **resolve the path fully, then check that the result is inside an allowed
//! root.** Checking the string *before* resolving is the classic mistake —
//! `../`, symlinks, and `~` all let a benign-looking string land somewhere else.
//!
//! Because a file being *created* does not exist yet, we resolve the nearest
//! existing ancestor and re-attach the remainder, so a symlinked parent
//! directory cannot be used to escape on write either.

use std::path::{Component, Path, PathBuf};

/// Why a path was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    /// Resolved outside every allowed root.
    Escapes(String),
    /// Matched a denied pattern (credential stores and similar).
    Denied(String),
    /// Could not be resolved at all.
    Invalid(String),
}

impl PathError {
    pub fn message(&self) -> String {
        match self {
            PathError::Escapes(p) => {
                format!("path '{p}' resolves outside the allowed roots (fs.roots)")
            }
            PathError::Denied(p) => format!("path '{p}' is denied by policy (fs.deny)"),
            PathError::Invalid(p) => format!("path '{p}' could not be resolved"),
        }
    }
}

/// Substrings that are refused anywhere in a resolved path.
///
/// These hold credentials or authentication state; the agent has no business
/// reading them through a general-purpose file tool, and exfiltrating them is
/// the obvious first move after a prompt injection.
pub fn default_denied() -> Vec<String> {
    [
        "/.ssh/",
        "/.aws/",
        "/.gnupg/",
        "/.kube/",
        "/.docker/config.json",
        "/Library/Keychains/",
        "/.netrc",
        "/.npmrc",
        "/.pypirc",
        "/.git-credentials",
        "/shadow",
        "/sudoers",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// A set of roots an agent may touch, plus denied substrings.
#[derive(Debug, Clone)]
pub struct Jail {
    roots: Vec<PathBuf>,
    denied: Vec<String>,
}

impl Jail {
    /// Build from configured roots. Roots are canonicalised once up front, so a
    /// symlinked root is compared in its real form.
    ///
    /// Denied patterns are lowercased here and matched case-insensitively.
    /// macOS and Windows both ship case-insensitive filesystems by default, so
    /// `~/.SSH/id_rsa` opens the very same file as `~/.ssh/id_rsa`; a
    /// case-sensitive comparison would be a one-keystroke bypass of the
    /// credential deny-list. On a case-sensitive volume this can deny a
    /// directory genuinely named `.SSH` — a harmless refusal, and the right way
    /// to be wrong.
    pub fn new(roots: Vec<PathBuf>, denied: Vec<String>) -> Self {
        let roots = roots
            .into_iter()
            .map(|r| std::fs::canonicalize(&r).unwrap_or(r))
            .collect();
        let denied = denied.iter().map(|d| d.to_lowercase()).collect();
        Jail { roots, denied }
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Is any root configured? With none, the engine refuses everything rather
    /// than defaulting to the whole disk.
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// Resolve `input` and confirm it is contained.
    ///
    /// Returns the fully-resolved path on success — callers must use *that*,
    /// never the original string, or the check is decorative.
    pub fn resolve(&self, input: &str) -> Result<PathBuf, PathError> {
        if input.is_empty() {
            return Err(PathError::Invalid(input.into()));
        }
        // `~` is not expanded: it would silently widen scope beyond the roots.
        if input.starts_with('~') {
            return Err(PathError::Invalid(format!(
                "{input} (tilde is not expanded; give an absolute path inside a root)"
            )));
        }
        let raw = PathBuf::from(input);
        let base = if raw.is_absolute() {
            raw
        } else {
            // Relative paths resolve against the first root, never the process
            // cwd, which the agent does not control and should not inherit.
            let root = self.roots.first().ok_or(PathError::Escapes(input.into()))?;
            root.join(raw)
        };

        let resolved = resolve_existing_prefix(&base)?;

        // Interior NUL or other oddities that survived resolution.
        let as_str = resolved.to_string_lossy().to_string();
        if as_str.contains('\0') {
            return Err(PathError::Invalid(input.into()));
        }
        // Compare with a trailing slash so `/.ssh/` also matches the directory
        // itself, and lowercased so a case-insensitive filesystem cannot alias
        // its way past the list (see `Jail::new`).
        let hay = format!("{as_str}/").to_lowercase();
        for pat in &self.denied {
            if hay.contains(pat.as_str()) {
                return Err(PathError::Denied(input.into()));
            }
        }
        if !self.roots.iter().any(|r| resolved.starts_with(r)) {
            return Err(PathError::Escapes(input.into()));
        }
        Ok(resolved)
    }
}

/// Canonicalise as much of `p` as exists, then re-attach the rest.
///
/// `std::fs::canonicalize` fails outright on a missing path, but writes and
/// mkdir legitimately target paths that do not exist yet. Resolving the
/// existing prefix still defeats a symlinked parent, which is the escape that
/// matters.
fn resolve_existing_prefix(p: &Path) -> Result<PathBuf, PathError> {
    if let Ok(c) = std::fs::canonicalize(p) {
        return Ok(c);
    }
    let mut existing = p.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        match existing.parent() {
            Some(parent) => {
                let name = existing
                    .file_name()
                    .ok_or_else(|| PathError::Invalid(p.display().to_string()))?
                    .to_os_string();
                tail.push(name);
                existing = parent.to_path_buf();
                if let Ok(c) = std::fs::canonicalize(&existing) {
                    let mut out = c;
                    for seg in tail.iter().rev() {
                        // Reject traversal segments in the unresolved tail;
                        // nothing legitimate needs them here.
                        if seg == ".." {
                            return Err(PathError::Escapes(p.display().to_string()));
                        }
                        out.push(seg);
                    }
                    return Ok(out);
                }
            }
            None => return Err(PathError::Invalid(p.display().to_string())),
        }
        if existing.components().next().is_none() {
            return Err(PathError::Invalid(p.display().to_string()));
        }
    }
}

/// Reject paths containing `..` before they are even resolved. Cheap defence in
/// depth; resolution is what actually enforces containment.
pub fn has_traversal(p: &str) -> bool {
    Path::new(p)
        .components()
        .any(|c| matches!(c, Component::ParentDir))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("mcp-fs-jail-{}", std::process::id()));
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("sub/ok.txt"), b"hi").unwrap();
        std::fs::canonicalize(d).unwrap()
    }

    fn jail() -> (Jail, PathBuf) {
        let root = tmp();
        (Jail::new(vec![root.clone()], default_denied()), root)
    }

    #[test]
    fn allows_a_path_inside_the_root() {
        let (j, root) = jail();
        let p = j
            .resolve(root.join("sub/ok.txt").to_str().unwrap())
            .unwrap();
        assert!(p.starts_with(&root));
    }

    #[test]
    fn blocks_dot_dot_traversal_out_of_the_root() {
        let (j, root) = jail();
        let escape = root.join("sub/../../../../etc/passwd");
        assert!(matches!(
            j.resolve(escape.to_str().unwrap()),
            Err(PathError::Escapes(_))
        ));
        assert!(has_traversal("a/../../b"));
        assert!(!has_traversal("a/b/c"));
    }

    #[test]
    fn blocks_absolute_paths_outside_the_root() {
        let (j, _) = jail();
        assert!(matches!(
            j.resolve("/etc/passwd"),
            Err(PathError::Escapes(_))
        ));
    }

    /// A symlink pointing out of the jail must not become an exit.
    #[test]
    #[cfg(unix)]
    fn blocks_symlink_escape() {
        let (j, root) = jail();
        let link = root.join("escape-link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink("/etc", &link).unwrap();
        let target = link.join("passwd");
        assert!(
            matches!(
                j.resolve(target.to_str().unwrap()),
                Err(PathError::Escapes(_))
            ),
            "symlink must not escape the jail"
        );
        let _ = std::fs::remove_file(&link);
    }

    /// Writing through a symlinked *parent* is the escape that a naive
    /// "canonicalize only if it exists" check misses.
    #[test]
    #[cfg(unix)]
    fn blocks_symlinked_parent_on_a_path_that_does_not_exist_yet() {
        let (j, root) = jail();
        let link = root.join("outdir");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink("/tmp", &link).unwrap();
        let new_file = link.join("brand-new-file.txt");
        assert!(
            matches!(
                j.resolve(new_file.to_str().unwrap()),
                Err(PathError::Escapes(_))
            ),
            "a symlinked parent must not allow creating files outside the jail"
        );
        let _ = std::fs::remove_file(&link);
    }

    #[test]
    fn allows_creating_a_new_file_inside_the_root() {
        let (j, root) = jail();
        let p = j
            .resolve(root.join("sub/not-yet.txt").to_str().unwrap())
            .unwrap();
        assert!(p.starts_with(&root));
    }

    #[test]
    fn denies_credential_paths_even_inside_a_root() {
        let root = tmp();
        std::fs::create_dir_all(root.join(".ssh")).unwrap();
        let j = Jail::new(vec![root.clone()], default_denied());
        assert!(matches!(
            j.resolve(root.join(".ssh/id_rsa").to_str().unwrap()),
            Err(PathError::Denied(_))
        ));
    }

    #[test]
    fn tilde_is_refused_rather_than_silently_expanded() {
        let (j, _) = jail();
        assert!(matches!(j.resolve("~/secrets"), Err(PathError::Invalid(_))));
    }

    #[test]
    fn no_roots_means_nothing_is_allowed() {
        let j = Jail::new(vec![], default_denied());
        assert!(j.is_empty());
        assert!(j.resolve("/tmp/anything").is_err());
    }
}
