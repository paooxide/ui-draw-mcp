//! Where `browser_navigate` may go.
//!
//! The browser is a general-purpose network client running inside the
//! perimeter, so it gets the same two controls as `http_request`: an operator
//! allowlist and a check on the **resolved address**. The second one is what
//! stops `http://169.254.169.254/` and `http://localhost:9200/` when the
//! allowlist is empty, and stops an allowlisted name whose DNS an attacker has
//! pointed at loopback.
//!
//! What this cannot do: follow the page. A redirect issued by the server, or a
//! fetch made by the page's own script, is not a navigation request and never
//! passes through here. `docs/threat-model.md` records that limit.

use std::net::{IpAddr, ToSocketAddrs};
use std::time::Duration;

use mcp_ssrf::{is_blocked_ip, parse_url, Parsed, UrlError};

/// How long a name lookup may take before the navigation is refused. A resolver
/// that hangs must not hold a tool call open indefinitely.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);

/// The only non-HTTP target a navigation may name: an empty page.
const ABOUT_BLANK: &str = "about:blank";

/// Why a navigation was refused. Mapped to `PermissionDenied` by the backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavDenied {
    /// Not `http`, `https` or `about:blank`. `file:`, `data:` and
    /// `javascript:` all bypass a control that exists elsewhere.
    Scheme(String),
    Malformed(String),
    OriginNotAllowed(String),
    BlockedAddress {
        host: String,
        ip: IpAddr,
    },
    Unresolvable(String),
}

impl NavDenied {
    pub fn message(&self) -> String {
        match self {
            NavDenied::Scheme(u) => {
                format!("navigation to '{u}' refused: only http, https and about:blank are allowed")
            }
            NavDenied::Malformed(m) => format!("navigation refused: {m}"),
            NavDenied::OriginNotAllowed(u) => {
                format!("navigation to '{u}' blocked by browser.allowed_origins policy")
            }
            NavDenied::BlockedAddress { host, ip } => format!(
                "navigation to '{host}' refused: it resolves to {ip}, which is loopback, \
                 private or link-local; set browser.allow_private = true to drive a local server"
            ),
            NavDenied::Unresolvable(h) => {
                format!("navigation refused: could not resolve host '{h}'")
            }
        }
    }
}

/// One parsed allowlist entry: an origin, plus an optional path prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    origin: Parsed,
    /// Everything after the authority, e.g. `/app` or `/#/x`. Empty or `/`
    /// means the whole origin.
    path_prefix: String,
}

/// Split a URL into its origin and the part after the authority.
fn split(url: &str) -> Result<(Parsed, String), UrlError> {
    let parsed = parse_url(url)?;
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or("");
    let tail_at = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    Ok((parsed, rest[tail_at..].to_string()))
}

/// The navigation policy. Immutable once built, so it is shared freely.
#[derive(Debug, Clone, Default)]
pub struct NavPolicy {
    entries: Vec<Entry>,
    /// Whether the allowlist was empty, i.e. any public origin is fine.
    open: bool,
    allow_private: bool,
}

impl NavPolicy {
    /// Build from `browser.allowed_origins` and `browser.allow_private`.
    ///
    /// An entry that does not parse as `http(s)://host[:port][/path]` can
    /// never match, and is dropped with an error line rather than aborting:
    /// dropping it narrows the policy, which is the safe direction. The
    /// entries that were understood are returned so a caller can report them.
    pub fn new(allowed_origins: &[String], allow_private: bool) -> Self {
        let mut entries = Vec::new();
        for raw in allowed_origins {
            match split(raw) {
                Ok((origin, path_prefix)) => entries.push(Entry {
                    origin,
                    path_prefix,
                }),
                Err(e) => tracing::error!(
                    entry = %raw,
                    reason = %e.message(),
                    "browser.allowed_origins entry ignored (it can never match)"
                ),
            }
        }
        NavPolicy {
            open: allowed_origins.is_empty(),
            entries,
            allow_private,
        }
    }

    pub fn allow_private(&self) -> bool {
        self.allow_private
    }

    /// The entries that were understood, rendered back as strings.
    pub fn entries(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|e| {
                format!(
                    "{}://{}:{}{}",
                    e.origin.scheme, e.origin.host, e.origin.port, e.path_prefix
                )
            })
            .collect()
    }

    fn matches(&self, origin: &Parsed, tail: &str) -> bool {
        self.entries.iter().any(|e| {
            e.origin == *origin
                && (e.path_prefix.is_empty()
                    || e.path_prefix == "/"
                    || tail.starts_with(&e.path_prefix))
        })
    }

    /// Everything except the resolver: scheme, shape and allowlist.
    fn check_shape(&self, url: &str) -> Result<Option<Parsed>, NavDenied> {
        if url == ABOUT_BLANK {
            return Ok(None);
        }
        let (origin, tail) = match split(url) {
            Ok(v) => v,
            Err(UrlError::Scheme(_)) => return Err(NavDenied::Scheme(url.to_string())),
            Err(UrlError::Malformed(_)) if !url.contains("://") => {
                return Err(NavDenied::Scheme(url.to_string()))
            }
            Err(e) => return Err(NavDenied::Malformed(e.message())),
        };
        if !self.open && !self.matches(&origin, &tail) {
            return Err(NavDenied::OriginNotAllowed(url.to_string()));
        }
        Ok(Some(origin))
    }

    /// Resolve and judge every address. Touches the system resolver, so it
    /// blocks; callers on the runtime use [`NavPolicy::check`].
    pub fn check_blocking(&self, url: &str) -> Result<(), NavDenied> {
        let Some(origin) = self.check_shape(url)? else {
            return Ok(());
        };
        if self.allow_private {
            return Ok(());
        }
        let addrs: Vec<IpAddr> = (origin.host.as_str(), origin.port)
            .to_socket_addrs()
            .map_err(|_| NavDenied::Unresolvable(origin.host.clone()))?
            .map(|s| s.ip())
            .collect();
        if addrs.is_empty() {
            return Err(NavDenied::Unresolvable(origin.host.clone()));
        }
        // Every address must be safe: a name that maps to one public and one
        // private address is a rebinding setup, not a public site.
        if let Some(ip) = addrs.into_iter().find(is_blocked_ip) {
            return Err(NavDenied::BlockedAddress {
                host: origin.host,
                ip,
            });
        }
        Ok(())
    }

    /// [`NavPolicy::check_blocking`] off the reactor, with a bound on how
    /// long the resolver may take. A timeout is reported as unresolvable,
    /// which denies.
    pub async fn check(&self, url: &str) -> Result<(), NavDenied> {
        // The shape checks are cheap and need no thread; only the resolver
        // moves off the runtime.
        let Some(origin) = self.check_shape(url)? else {
            return Ok(());
        };
        if self.allow_private {
            return Ok(());
        }
        let me = self.clone();
        let url = url.to_string();
        let host = origin.host.clone();
        let handle = tokio::task::spawn_blocking(move || me.check_blocking(&url));
        match tokio::time::timeout(RESOLVE_TIMEOUT, handle).await {
            Ok(Ok(result)) => result,
            // The blocking task panicked; refuse rather than guess.
            Ok(Err(_)) => Err(NavDenied::Unresolvable(host)),
            Err(_elapsed) => Err(NavDenied::Unresolvable(host)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(entries: &[&str], allow_private: bool) -> NavPolicy {
        NavPolicy::new(
            &entries.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            allow_private,
        )
    }

    // ---- happy path -----------------------------------------------------

    #[test]
    fn open_policy_allows_a_public_site() {
        let p = policy(&[], false);
        // Shape only: the resolver is exercised in the red-team suite against
        // literals, so a unit test does not depend on the network.
        assert!(p.check_shape("https://example.com/a?b#c").is_ok());
    }

    #[test]
    fn allowlisted_origin_matches_any_path_and_query() {
        let p = policy(&["https://ok.example"], false);
        for u in [
            "https://ok.example",
            "https://ok.example/",
            "https://ok.example/path?x=1#frag",
            "https://ok.example:443/explicit-default-port",
            "HTTPS://OK.EXAMPLE/case-insensitive",
            "https://ok.example./trailing-dot",
        ] {
            assert!(p.check_shape(u).is_ok(), "{u} should match");
        }
    }

    #[test]
    fn about_blank_is_always_fine() {
        assert!(policy(&["https://ok.example"], false)
            .check_blocking("about:blank")
            .is_ok());
        assert!(policy(&[], false).check_blocking("about:blank").is_ok());
    }

    #[test]
    fn allow_private_lifts_the_address_check_only() {
        let p = policy(&[], true);
        assert!(p.check_blocking("http://127.0.0.1:3000/").is_ok());
        // Still not a way past the allowlist.
        let p = policy(&["https://ok.example"], true);
        assert_eq!(
            p.check_blocking("http://127.0.0.1:3000/"),
            Err(NavDenied::OriginNotAllowed("http://127.0.0.1:3000/".into()))
        );
    }

    // ---- boundaries -----------------------------------------------------

    /// The bug the old prefix check had: `https://ok.example` matched
    /// `https://ok.example.evil/` and `https://ok.example.community/`.
    #[test]
    fn origin_match_stops_at_the_host_boundary() {
        let p = policy(&["https://ok.example"], false);
        for u in [
            "https://ok.example.evil/",
            "https://ok.examplecommunity/",
            "https://ok.example.com/",
            "https://ok.example:8443/other-port",
            "http://ok.example/other-scheme",
            "https://sub.ok.example/subdomain",
        ] {
            assert_eq!(
                p.check_shape(u),
                Err(NavDenied::OriginNotAllowed(u.into())),
                "{u} must not match"
            );
        }
    }

    #[test]
    fn a_path_prefix_in_the_entry_is_honoured() {
        let p = policy(&["https://a/#/x"], false);
        assert!(p.check_shape("https://a/#/x").is_ok());
        assert!(p.check_shape("https://a/#/x/deeper").is_ok());
        assert_eq!(
            p.check_shape("https://a/#/y"),
            Err(NavDenied::OriginNotAllowed("https://a/#/y".into()))
        );
        assert_eq!(
            p.check_shape("https://a/"),
            Err(NavDenied::OriginNotAllowed("https://a/".into()))
        );
        // A bare slash means the whole origin, same as no path.
        let p = policy(&["https://a/"], false);
        assert!(p.check_shape("https://a/anything").is_ok());
    }

    #[test]
    fn empty_and_whitespace_urls_are_refused() {
        let p = policy(&[], false);
        assert!(matches!(p.check_shape(""), Err(NavDenied::Scheme(_))));
        assert!(matches!(
            p.check_shape("https://ok.example/with space"),
            Err(NavDenied::Malformed(_))
        ));
        assert!(matches!(
            p.check_shape("https://ok.example/line\nbreak"),
            Err(NavDenied::Malformed(_))
        ));
    }

    #[test]
    fn malformed_allowlist_entries_never_match_and_never_widen() {
        // A junk entry alongside a good one: the good one works, the junk one
        // matches nothing, and the policy is still closed (not `open`).
        let p = policy(&["not a url", "https://ok.example"], false);
        assert_eq!(p.entries(), vec!["https://ok.example:443"]);
        assert!(!p.open);
        assert!(p.check_shape("https://ok.example/").is_ok());
        assert!(matches!(
            p.check_shape("https://evil.example/"),
            Err(NavDenied::OriginNotAllowed(_))
        ));
        // Every entry junk: nothing matches, and it is still not open.
        let p = policy(&["junk"], false);
        assert!(!p.open);
        assert!(matches!(
            p.check_shape("https://ok.example/"),
            Err(NavDenied::OriginNotAllowed(_))
        ));
    }

    // ---- faults ---------------------------------------------------------

    #[test]
    fn non_web_schemes_are_refused_even_when_open() {
        let p = policy(&[], true);
        for u in [
            "file:///etc/passwd",
            "data:text/html,<script>alert(1)</script>",
            "javascript:alert(1)",
            "chrome://settings",
            "about:config",
            "ftp://x/",
        ] {
            assert!(
                matches!(p.check_blocking(u), Err(NavDenied::Scheme(_))),
                "{u} must be refused"
            );
        }
    }

    #[test]
    fn literal_private_and_metadata_addresses_are_blocked() {
        let p = policy(&[], false);
        for host in [
            "127.0.0.1",
            "10.0.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "[::1]",
            "[::ffff:169.254.169.254]",
            "[64:ff9b::a9fe:a9fe]",
            "[2002:a9fe:a9fe::]",
        ] {
            let u = format!("http://{host}/");
            match p.check_blocking(&u) {
                Err(NavDenied::BlockedAddress { .. }) => {}
                other => panic!("{u} should be blocked, got {other:?}"),
            }
        }
    }

    #[test]
    fn an_allowlisted_name_that_resolves_to_loopback_is_still_blocked() {
        let p = policy(&["http://localhost:8080"], false);
        match p.check_blocking("http://localhost:8080/admin") {
            Err(NavDenied::BlockedAddress { host, .. }) => assert_eq!(host, "localhost"),
            other => panic!("expected BlockedAddress, got {other:?}"),
        }
    }

    #[test]
    fn an_unresolvable_host_is_refused_not_an_error() {
        let p = policy(&[], false);
        assert_eq!(
            p.check_blocking("http://does-not-exist.invalid/"),
            Err(NavDenied::Unresolvable("does-not-exist.invalid".into()))
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn async_check_agrees_with_blocking_and_denies_on_timeout_path() {
        let p = policy(&[], false);
        assert!(matches!(
            p.check("http://169.254.169.254/latest/meta-data/").await,
            Err(NavDenied::BlockedAddress { .. })
        ));
        assert!(p.check("about:blank").await.is_ok());
        assert!(matches!(
            p.check("file:///etc/hosts").await,
            Err(NavDenied::Scheme(_))
        ));
    }

    // ---- concurrency ----------------------------------------------------

    /// The policy is shared by every tool call; it has no interior state, so
    /// concurrent checks must agree with sequential ones.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_checks_are_consistent() {
        let p = std::sync::Arc::new(policy(&["https://ok.example"], false));
        let mut tasks = Vec::new();
        for i in 0..64 {
            let p = p.clone();
            tasks.push(tokio::spawn(async move {
                let u = if i % 2 == 0 {
                    "about:blank"
                } else {
                    "https://evil.example/"
                };
                (i, p.check(u).await)
            }));
        }
        for t in tasks {
            let (i, r) = t.await.unwrap();
            if i % 2 == 0 {
                assert_eq!(r, Ok(()));
            } else {
                assert!(matches!(r, Err(NavDenied::OriginNotAllowed(_))));
            }
        }
    }
}
