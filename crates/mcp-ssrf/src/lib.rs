//! SSRF containment for anything that reaches the network on the agent's
//! behalf: `http_request`, the host probes, and `browser_navigate`.
//!
//! An agent that can make arbitrary requests from *this* machine is inside
//! the network perimeter. The classic abuse is not fetching a public page. It
//! is `http://169.254.169.254/` (cloud instance credentials), `http://localhost:*`
//! (admin panels, unauthenticated internal services), or an RFC1918 address.
//!
//! Defence has to happen on the **resolved address**, not the hostname: an
//! attacker controls DNS, so `evil.example` can resolve to `127.0.0.1`. We
//! resolve first and check every address the name maps to.
//!
//! This is a leaf crate with no dependencies so that every engine can use the
//! same guard without one engine depending on another (see the layering rules
//! in `docs/architecture.md`).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlError {
    Malformed(String),
    Scheme(String),
    HostNotAllowed(String),
    BlockedAddress(String),
    Unresolvable(String),
}

impl UrlError {
    pub fn message(&self) -> String {
        match self {
            UrlError::Malformed(u) => format!("malformed URL '{u}'"),
            UrlError::Scheme(s) => format!("scheme '{s}' is not allowed (use http or https)"),
            UrlError::HostNotAllowed(h) => {
                format!("host '{h}' is not in network.allowed_hosts")
            }
            UrlError::BlockedAddress(a) => format!(
                "'{a}' is a private, loopback, or link-local address, blocked to prevent \
                 access to internal services and cloud metadata"
            ),
            UrlError::Unresolvable(h) => format!("could not resolve host '{h}'"),
        }
    }
}

/// The pieces of a URL we care about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub scheme: String,
    pub host: String,
    pub port: u16,
}

/// Minimal URL split, enough to extract scheme/host/port for the guard.
/// Deliberately strict: anything unusual is rejected rather than guessed at.
pub fn parse_url(url: &str) -> Result<Parsed, UrlError> {
    // A URL carrying whitespace or a control character is either malformed or
    // an injection attempt (CR/LF splits headers in whatever speaks the wire
    // protocol downstream). Refuse it here rather than trusting every consumer
    // to re-sanitise.
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(UrlError::Malformed(format!(
            "{} (whitespace and control characters are not accepted)",
            url.escape_debug()
        )));
    }
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| UrlError::Malformed(url.into()))?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(UrlError::Scheme(scheme));
    }
    // Strip path/query/fragment.
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| UrlError::Malformed(url.into()))?;
    // Credentials in the URL are a redirect/parsing footgun; refuse them.
    if authority.contains('@') {
        return Err(UrlError::Malformed(format!(
            "{url} (credentials in the URL are not accepted)"
        )));
    }
    let default_port = if scheme == "https" { 443 } else { 80 };
    let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
        // IPv6 literal
        let (h, tail) = stripped
            .split_once(']')
            .ok_or_else(|| UrlError::Malformed(url.into()))?;
        let port = tail
            .strip_prefix(':')
            .map(|p| {
                p.parse::<u16>()
                    .map_err(|_| UrlError::Malformed(url.into()))
            })
            .transpose()?
            .unwrap_or(default_port);
        (h.to_string(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (
                h.to_string(),
                p.parse::<u16>()
                    .map_err(|_| UrlError::Malformed(url.into()))?,
            ),
            None => (authority.to_string(), default_port),
        }
    };
    // A fully-qualified name ("example.com.") is the same host as its bare
    // form; normalise so allowlist comparison is not spelling-sensitive.
    let host = host.strip_suffix('.').unwrap_or(&host).to_ascii_lowercase();
    if host.is_empty() {
        return Err(UrlError::Malformed(url.into()));
    }
    // Non-ASCII hosts are a homograph problem: `exa\u{43c}ple.com` reads like
    // `example.com` but is a different name. The allowlist is ASCII, so these
    // could only ever fail closed, but they should fail with a clear reason
    // rather than as an unresolvable name. Punycode is still accepted.
    if !host.is_ascii() {
        return Err(UrlError::Malformed(format!(
            "{url} (non-ASCII hostname; use its punycode form)"
        )));
    }
    Ok(Parsed { scheme, host, port })
}

/// Is this address one an agent must never be able to reach?
pub fn is_blocked_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => is_blocked_v6(v6),
    }
}

fn is_blocked_v4(ip: &Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_loopback()            // 127/8
        || ip.is_private()      // 10/8, 172.16/12, 192.168/16
        || ip.is_link_local()   // 169.254/16, cloud metadata lives here
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()  // 0.0.0.0
        || o[0] == 0
        || (o[0] == 100 && (64..128).contains(&o[1])) // 100.64/10 CGNAT
        || o[0] >= 224 // multicast + reserved
}

fn is_blocked_v6(ip: &Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return true;
    }
    let seg = ip.segments();
    // fc00::/7 unique-local, fe80::/10 link-local
    if (seg[0] & 0xfe00) == 0xfc00 || (seg[0] & 0xffc0) == 0xfe80 {
        return true;
    }
    // IPv4-mapped/compatible: re-check the embedded v4 address, otherwise
    // ::ffff:127.0.0.1 would sail straight through.
    if let Some(v4) = ip.to_ipv4() {
        return is_blocked_v4(&v4);
    }
    // 100::/64 discard-only, 2001:db8::/32 documentation, 2001::/32 Teredo
    // (which tunnels through a relay we have no visibility into).
    if seg[0] == 0x0100 && seg[1] == 0 && seg[2] == 0 && seg[3] == 0 {
        return true;
    }
    if seg[0] == 0x2001 && (seg[1] == 0x0db8 || seg[1] == 0x0000) {
        return true;
    }
    // The transition mechanisms below carry an IPv4 address *inside* an IPv6
    // one. Checking only the outer form would let `64:ff9b::7f00:1` and
    // `2002:a9fe:a9fe::` reach loopback and cloud metadata respectively, since
    // the kernel translates them back on the way out.
    if let Some(v4) = embedded_v4(&seg) {
        return is_blocked_v4(&v4);
    }
    // 64:ff9b:1::/48 is local-use NAT64 (RFC 8215): the embedded address sits
    // at a prefix-dependent offset we cannot know, and a local-use translation
    // prefix is never a legitimate public destination. Refuse the range.
    if seg[0] == 0x0064 && seg[1] == 0xff9b && seg[2] == 0x0001 {
        return true;
    }
    false
}

/// Extract the IPv4 address embedded by a transition mechanism, if any.
///
/// `64:ff9b::/96` is the well-known NAT64 prefix (RFC 6052) and `2002::/16` is
/// 6to4 (RFC 3056). Both are legitimate ways to reach *public* IPv4, so the
/// embedded address is judged on its own merits rather than the range being
/// blocked outright, otherwise a NAT64-only network could not reach anything.
fn embedded_v4(seg: &[u16; 8]) -> Option<Ipv4Addr> {
    let from = |hi: u16, lo: u16| {
        Ipv4Addr::new(
            (hi >> 8) as u8,
            (hi & 0xff) as u8,
            (lo >> 8) as u8,
            (lo & 0xff) as u8,
        )
    };
    // NAT64 well-known prefix 64:ff9b::/96
    if seg[0] == 0x0064 && seg[1] == 0xff9b && seg[2..6] == [0, 0, 0, 0] {
        return Some(from(seg[6], seg[7]));
    }
    // 6to4 2002::/16
    if seg[0] == 0x2002 {
        return Some(from(seg[1], seg[2]));
    }
    None
}

/// Policy for outbound requests.
#[derive(Debug, Clone, Default)]
pub struct NetPolicy {
    /// Exact hostnames (or `.suffix` entries) the agent may reach. Empty means
    /// **nothing** is reachable: the engine refuses rather than opening the
    /// whole internet by default.
    pub allowed_hosts: Vec<String>,
    /// Permit private/loopback targets. Off by default; only sensible for a
    /// deliberately sandboxed deployment.
    pub allow_private: bool,
}

impl NetPolicy {
    pub fn host_allowed(&self, host: &str) -> bool {
        self.allowed_hosts.iter().any(|a| {
            let a = a.to_ascii_lowercase();
            if let Some(suffix) = a.strip_prefix('.') {
                host == suffix || host.ends_with(&format!(".{suffix}"))
            } else {
                host == a
            }
        })
    }

    /// Host-level check for tools that reach a host without a URL (ping,
    /// traceroute, tcp_connect). Same rule as [`Self::check`]: the allowlist,
    /// then every address the name resolves to. A probe is still a reach-out,
    /// and "just a ping" to a metadata address is still a reach-out to it.
    pub fn check_host(&self, host: &str) -> Result<Vec<IpAddr>, String> {
        let host = host.to_ascii_lowercase();
        if self.allowed_hosts.is_empty() || !self.host_allowed(&host) {
            return Err(format!("host '{host}' is not in network.allowed_hosts"));
        }
        let addrs: Vec<IpAddr> = (host.as_str(), 0u16)
            .to_socket_addrs()
            .map_err(|_| format!("could not resolve '{host}'"))?
            .map(|s| s.ip())
            .collect();
        if addrs.is_empty() {
            return Err(format!("could not resolve '{host}'"));
        }
        if !self.allow_private {
            if let Some(bad) = addrs.iter().find(|a| is_blocked_ip(a)) {
                return Err(format!(
                    "'{host}' resolves to {bad}, which is loopback/private/link-local"
                ));
            }
        }
        Ok(addrs)
    }

    /// Full check: scheme, host allowlist, then **every** resolved address.
    ///
    /// Returns the resolved addresses so the caller can pin the connection to a
    /// checked IP rather than re-resolving (which would reopen a DNS-rebinding
    /// window between check and use).
    pub fn check(&self, url: &str) -> Result<(Parsed, Vec<IpAddr>), UrlError> {
        let parsed = parse_url(url)?;
        if self.allowed_hosts.is_empty() || !self.host_allowed(&parsed.host) {
            return Err(UrlError::HostNotAllowed(parsed.host));
        }
        let addrs: Vec<IpAddr> = (parsed.host.as_str(), parsed.port)
            .to_socket_addrs()
            .map_err(|_| UrlError::Unresolvable(parsed.host.clone()))?
            .map(|s| s.ip())
            .collect();
        if addrs.is_empty() {
            return Err(UrlError::Unresolvable(parsed.host.clone()));
        }
        if !self.allow_private {
            // *Every* address must be safe: a name resolving to both a public
            // and a private address must not be usable.
            if let Some(bad) = addrs.iter().find(|a| is_blocked_ip(a)) {
                return Err(UrlError::BlockedAddress(bad.to_string()));
            }
        }
        Ok((parsed, addrs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn parses_scheme_host_and_port() {
        let p = parse_url("https://example.com/a/b?c=d").unwrap();
        assert_eq!(p.scheme, "https");
        assert_eq!(p.host, "example.com");
        assert_eq!(p.port, 443);
        assert_eq!(parse_url("http://example.com").unwrap().port, 80);
        assert_eq!(parse_url("http://example.com:8080/x").unwrap().port, 8080);
        assert_eq!(parse_url("http://[::1]:9000/").unwrap().host, "::1");
    }

    #[test]
    fn rejects_non_http_schemes_and_junk() {
        assert!(matches!(
            parse_url("file:///etc/passwd"),
            Err(UrlError::Scheme(_))
        ));
        assert!(matches!(parse_url("gopher://x/"), Err(UrlError::Scheme(_))));
        assert!(matches!(
            parse_url("not a url"),
            Err(UrlError::Malformed(_))
        ));
        // Credentials are a redirect-parsing footgun.
        assert!(matches!(
            parse_url("http://user:pw@example.com/"),
            Err(UrlError::Malformed(_))
        ));
    }

    /// The addresses that matter most.
    #[test]
    fn blocks_metadata_loopback_and_private_addresses() {
        for bad in [
            "169.254.169.254", // cloud instance metadata
            "127.0.0.1",
            "0.0.0.0",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "100.64.0.1", // CGNAT
            "::1",
            "fe80::1",
            "fd00::1",
            "::ffff:127.0.0.1", // IPv4-mapped loopback
        ] {
            let ip = IpAddr::from_str(bad).unwrap();
            assert!(is_blocked_ip(&ip), "{bad} should be blocked");
        }
    }

    #[test]
    fn allows_ordinary_public_addresses() {
        for ok in ["1.1.1.1", "93.184.216.34", "2606:2800:220:1::1"] {
            let ip = IpAddr::from_str(ok).unwrap();
            assert!(!is_blocked_ip(&ip), "{ok} should be allowed");
        }
    }

    #[test]
    fn host_allowlist_supports_exact_and_suffix() {
        let p = NetPolicy {
            allowed_hosts: vec!["example.com".into(), ".corp.internal".into()],
            allow_private: false,
        };
        assert!(p.host_allowed("example.com"));
        assert!(!p.host_allowed("evil.com"));
        assert!(!p.host_allowed("notexample.com"));
        assert!(p.host_allowed("corp.internal"));
        assert!(p.host_allowed("api.corp.internal"));
        assert!(!p.host_allowed("corp.internal.evil.com"));
    }

    #[test]
    fn empty_allowlist_reaches_nothing() {
        let p = NetPolicy::default();
        assert!(matches!(
            p.check("https://example.com/"),
            Err(UrlError::HostNotAllowed(_))
        ));
    }

    /// DNS is attacker-controlled, so an allowlisted name that resolves to
    /// loopback must still be refused.
    #[test]
    fn allowlisted_host_resolving_to_loopback_is_still_blocked() {
        let p = NetPolicy {
            allowed_hosts: vec!["localhost".into()],
            allow_private: false,
        };
        match p.check("http://localhost:80/") {
            Err(UrlError::BlockedAddress(_)) => {}
            other => panic!("expected BlockedAddress, got {other:?}"),
        }
    }

    #[test]
    fn private_targets_are_reachable_only_when_explicitly_permitted() {
        let p = NetPolicy {
            allowed_hosts: vec!["localhost".into()],
            allow_private: true,
        };
        assert!(p.check("http://localhost:80/").is_ok());
    }
}
