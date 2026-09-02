//! What may be installed, from where, and what may never be removed.
//!
//! This is the most dangerous category in the product: an install is arbitrary
//! code execution *plus* persistence, run by a manager that often has elevated
//! rights. Everything here is closed by default.

/// A package identifier that is safe to pass to a package manager as argv.
///
/// The check that matters is the leading `-`: `brew install --force` reaching
/// the manager as an "id" would disable exactly the verification this engine
/// promises never to bypass. Everything else keeps ids from carrying paths,
/// shell syntax, or option separators.
pub fn valid_package_id(id: &str) -> bool {
    if id.is_empty() || id.len() > 128 {
        return false;
    }
    if id.starts_with('-') {
        return false;
    }
    id.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '@' | '/'))
        && !id.contains("..")
        && !id.starts_with('/')
}

/// Version strings are argv too, and `--build-from-source` is a version-shaped
/// string only if nobody checks.
pub fn valid_version(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 64
        && !v.starts_with('-')
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+' | ':' | '~'))
}

/// Packages `app_uninstall` must always refuse.
///
/// An agent must not be able to remove its own supervision, the manager it
/// depends on, or the endpoint protection watching it. This is not
/// configurable — a deployment that could switch it off would not have it.
pub const PROTECTED: &[&str] = &[
    "agentctl",
    "brew",
    "homebrew",
    "mas",
    "xcode",
    "xcode-select",
    "command-line-tools",
    "macos",
    "darwin",
    "openssl",
    "ca-certificates",
    "curl",
    "git",
    "sudo",
    "coreutils",
    // Endpoint protection / management agents.
    "crowdstrike",
    "falcon",
    "sentinelone",
    "jamf",
    "osquery",
    "santa",
    "little-snitch",
    "lulu",
];

pub fn is_protected(id: &str) -> bool {
    let lower = id.to_ascii_lowercase();
    let leaf = lower.rsplit('/').next().unwrap_or(&lower);
    PROTECTED.iter().any(|p| {
        leaf == *p || leaf.starts_with(&format!("{p}-")) || leaf.starts_with(&format!("{p}@"))
    })
}

/// Deployment policy for the packages engine.
#[derive(Debug, Clone)]
pub struct PkgPolicy {
    /// Managers that may be used. Empty = the engine refuses everything.
    pub allowed_sources: Vec<String>,
    /// Permit installing from a URL, local file, or third-party tap. Off by
    /// default: that path is indistinguishable from malware delivery.
    pub allow_arbitrary_source: bool,
    /// If non-empty, only these package ids may be installed.
    pub allowlist: Vec<String>,
    /// Ids that may never be installed.
    pub denylist: Vec<String>,
    pub timeout_secs: u64,
}

impl Default for PkgPolicy {
    fn default() -> Self {
        PkgPolicy {
            allowed_sources: Vec::new(),
            allow_arbitrary_source: false,
            allowlist: Vec::new(),
            denylist: Vec::new(),
            timeout_secs: 600,
        }
    }
}

/// Why a package request was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    BadId(String),
    UnknownSource(String),
    ArbitrarySource(String),
    NotAllowlisted(String),
    Denylisted(String),
    Protected(String),
}

impl Refusal {
    pub fn message(&self) -> String {
        match self {
            Refusal::BadId(id) => format!(
                "'{id}' is not a valid package identifier (an id that starts with '-' would reach \
                 the package manager as an option)"
            ),
            Refusal::UnknownSource(s) => format!("source '{s}' is not in packages.allowed_sources"),
            Refusal::ArbitrarySource(s) => format!(
                "'{s}' names a URL, file or third-party tap; packages.allow_arbitrary_source is off"
            ),
            Refusal::NotAllowlisted(id) => format!("'{id}' is not in packages.allowlist"),
            Refusal::Denylisted(id) => format!("'{id}' is in packages.denylist"),
            Refusal::Protected(id) => format!(
                "'{id}' is protected and can never be uninstalled — an agent must not be able to \
                 remove its own supervision or the tooling that watches it"
            ),
        }
    }
}

impl PkgPolicy {
    pub fn check_source(&self, source: &str) -> Result<(), Refusal> {
        if !self.allowed_sources.iter().any(|s| s == source) {
            return Err(Refusal::UnknownSource(source.to_string()));
        }
        Ok(())
    }

    /// Does this id name something other than a plain package from the manager's
    /// own index?
    fn is_arbitrary(id: &str) -> bool {
        id.contains("://") || id.starts_with('/') || id.ends_with(".rb") || id.contains('/')
    }

    /// Full gate for a read-only lookup: identity and source only.
    pub fn check_lookup(&self, id: &str, source: &str) -> Result<(), Refusal> {
        if !valid_package_id(id) {
            return Err(Refusal::BadId(id.to_string()));
        }
        self.check_source(source)
    }

    /// Full gate for a mutation.
    pub fn check_install(&self, id: &str, source: &str) -> Result<(), Refusal> {
        self.check_lookup(id, source)?;
        if Self::is_arbitrary(id) && !self.allow_arbitrary_source {
            return Err(Refusal::ArbitrarySource(id.to_string()));
        }
        if self.denylist.iter().any(|d| d.eq_ignore_ascii_case(id)) {
            return Err(Refusal::Denylisted(id.to_string()));
        }
        if !self.allowlist.is_empty() && !self.allowlist.iter().any(|a| a.eq_ignore_ascii_case(id))
        {
            return Err(Refusal::NotAllowlisted(id.to_string()));
        }
        Ok(())
    }

    pub fn check_uninstall(&self, id: &str, source: &str) -> Result<(), Refusal> {
        self.check_lookup(id, source)?;
        if is_protected(id) {
            return Err(Refusal::Protected(id.to_string()));
        }
        if self.denylist.iter().any(|d| d.eq_ignore_ascii_case(id)) {
            return Err(Refusal::Denylisted(id.to_string()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of validating ids: an option smuggled in as a package
    /// name would turn off the verification this engine promises to keep.
    #[test]
    fn ids_that_would_become_options_are_rejected() {
        for bad in [
            "--force",
            "-f",
            "--no-verify",
            "--build-from-source",
            "",
            "/etc/passwd",
            "../../etc/passwd",
            "pkg;rm -rf /",
            "pkg$(whoami)",
            "pkg|sh",
            "pkg name",
        ] {
            assert!(!valid_package_id(bad), "{bad:?} must be rejected");
        }
        for good in [
            "wget",
            "node@20",
            "python3.12",
            "aws-cli",
            "homebrew/core/git",
        ] {
            assert!(valid_package_id(good), "{good:?} must be accepted");
        }
    }

    #[test]
    fn versions_cannot_smuggle_options_either() {
        assert!(valid_version("1.2.3"));
        assert!(valid_version("20.11.0_1"));
        assert!(!valid_version("--force"));
        assert!(!valid_version(""));
    }

    /// Uninstalling its own supervision is the one thing an agent must never be
    /// able to do, regardless of configuration.
    #[test]
    fn the_protected_set_covers_supervision_and_security_tooling() {
        for p in [
            "agentctl",
            "brew",
            "Homebrew",
            "openssl@3",
            "git",
            "crowdstrike-falcon",
            "santa",
            "jamf",
            "osquery",
        ] {
            assert!(is_protected(p), "{p} must be protected");
        }
        assert!(!is_protected("wget"));
        assert!(!is_protected("ripgrep"));
    }

    #[test]
    fn everything_is_closed_until_a_source_is_configured() {
        let p = PkgPolicy::default();
        assert_eq!(
            p.check_lookup("wget", "brew"),
            Err(Refusal::UnknownSource("brew".into()))
        );
    }

    #[test]
    fn arbitrary_sources_are_off_by_default() {
        let p = PkgPolicy {
            allowed_sources: vec!["brew".into()],
            ..PkgPolicy::default()
        };
        assert!(matches!(
            p.check_install("https://evil.example/pkg.rb", "brew"),
            Err(Refusal::BadId(_))
        ));
        assert_eq!(
            p.check_install("thirdparty/tap/thing", "brew"),
            Err(Refusal::ArbitrarySource("thirdparty/tap/thing".into()))
        );
        let open = PkgPolicy {
            allow_arbitrary_source: true,
            ..p.clone()
        };
        assert!(open.check_install("thirdparty/tap/thing", "brew").is_ok());
    }

    #[test]
    fn allowlist_and_denylist_bound_what_installs() {
        let p = PkgPolicy {
            allowed_sources: vec!["brew".into()],
            allowlist: vec!["ripgrep".into()],
            denylist: vec!["telnet".into()],
            ..PkgPolicy::default()
        };
        assert!(p.check_install("ripgrep", "brew").is_ok());
        assert_eq!(
            p.check_install("wget", "brew"),
            Err(Refusal::NotAllowlisted("wget".into()))
        );
        assert_eq!(
            p.check_install("telnet", "brew"),
            Err(Refusal::Denylisted("telnet".into()))
        );
    }

    #[test]
    fn protection_holds_even_with_an_empty_policy() {
        let p = PkgPolicy {
            allowed_sources: vec!["brew".into()],
            ..PkgPolicy::default()
        };
        assert_eq!(
            p.check_uninstall("agentctl", "brew"),
            Err(Refusal::Protected("agentctl".into()))
        );
        assert!(p.check_uninstall("ripgrep", "brew").is_ok());
    }
}
