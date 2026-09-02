//! Red-team suite for outbound-request containment.
//!
//! An agent with arbitrary HTTP from this machine is *inside* the network
//! perimeter. The interesting attacks are not "fetch a bad page" — they are
//! reaching `169.254.169.254` for cloud credentials, or an unauthenticated
//! admin panel on loopback, by dressing the address up so a string check misses
//! it. The guard's answer is to resolve first and judge the **address**; these
//! tests exercise that against the real system resolver.

use std::net::IpAddr;
use std::str::FromStr;

use mcp_net::{is_blocked_ip, parse_url, NetPolicy, UrlError};

fn policy_allowing(hosts: &[&str]) -> NetPolicy {
    NetPolicy {
        allowed_hosts: hosts.iter().map(|h| h.to_string()).collect(),
        allow_private: false,
    }
}

/// The headline case: alternate spellings of `127.0.0.1` that a substring
/// blocklist would miss. Each is allowlisted *by its literal spelling* so the
/// test reaches the address check rather than stopping at the allowlist —
/// otherwise it would pass for the wrong reason.
#[test]
fn alternate_ip_encodings_are_resolved_then_blocked() {
    for spelling in ["2130706433", "0x7f.0.0.1", "127.1", "127.0.0.1"] {
        let p = policy_allowing(&[spelling]);
        let url = format!("http://{spelling}/");
        match p.check(&url) {
            Err(UrlError::BlockedAddress(_)) => {}
            other => panic!("{spelling} should resolve to loopback and be blocked, got {other:?}"),
        }
    }
}

/// Cloud metadata by every route into it.
#[test]
fn cloud_metadata_endpoints_are_blocked() {
    for addr in [
        "169.254.169.254",        // AWS / GCP / Azure IMDS
        "169.254.170.2",          // ECS task metadata
        "fd00:ec2::254",          // AWS IMDSv6
        "::ffff:169.254.169.254", // IPv4-mapped
    ] {
        let ip = IpAddr::from_str(addr).unwrap();
        assert!(is_blocked_ip(&ip), "{addr} must be blocked");
    }
}

/// IPv6 transition mechanisms smuggle an IPv4 address inside an IPv6 one. If
/// the guard only understands native IPv6 it will wave these through and the
/// kernel will happily translate them back to loopback or the metadata address.
#[test]
fn ipv6_transition_addresses_cannot_smuggle_ipv4() {
    for addr in [
        "::ffff:127.0.0.1",   // IPv4-mapped
        "::127.0.0.1",        // IPv4-compatible (deprecated)
        "64:ff9b::7f00:1",    // NAT64 well-known prefix -> 127.0.0.1
        "64:ff9b::a9fe:a9fe", // NAT64 -> 169.254.169.254
        "64:ff9b:1::7f00:1",  // NAT64 local-use prefix
        "2002:7f00:1::",      // 6to4 -> 127.0.0.1
        "2002:a9fe:a9fe::",   // 6to4 -> 169.254.169.254
        "2001:0:0:0:0:0:0:1", // Teredo
        "100::1",             // discard-only
        "2001:db8::1",        // documentation
    ] {
        let ip = IpAddr::from_str(addr).unwrap();
        assert!(is_blocked_ip(&ip), "{addr} must be blocked");
    }
}

/// Guard against over-blocking: ordinary public IPv6 must still work, or the
/// engine is useless on a v6 network.
#[test]
fn ordinary_public_addresses_survive() {
    for addr in [
        "1.1.1.1",
        "8.8.8.8",
        "93.184.216.34",
        "2606:2800:220:1::1", // example.com
        "2620:fe::fe",        // Quad9
        "2a00:1450:4001::1",  // Google
    ] {
        let ip = IpAddr::from_str(addr).unwrap();
        assert!(!is_blocked_ip(&ip), "{addr} must NOT be blocked");
    }
}

/// Reserved ranges that are not routable but are reachable *locally* — the
/// point of an SSRF is to reach something the agent shouldn't.
#[test]
fn reserved_and_local_ranges_are_blocked() {
    for addr in [
        "0.0.0.0",
        "0.1.2.3", // 0/8
        "10.0.0.1",
        "172.16.0.1",
        "172.31.255.254",
        "192.168.0.1",
        "127.255.255.254",
        "100.64.0.1", // CGNAT
        "100.127.255.255",
        "224.0.0.1",       // multicast
        "255.255.255.255", // broadcast
        "::1",
        "fe80::1",
        "fd00::1",
        "ff02::1", // multicast
    ] {
        let ip = IpAddr::from_str(addr).unwrap();
        assert!(is_blocked_ip(&ip), "{addr} must be blocked");
    }
}

/// `user@host` is the oldest URL-confusion trick there is: readers see the
/// left-hand side, the client connects to the right.
#[test]
fn userinfo_in_url_is_refused() {
    for url in [
        "http://169.254.169.254@example.com/",
        "http://example.com@169.254.169.254/",
        "http://user:pw@example.com/",
        "http://example.com%40evil.com@127.0.0.1/",
    ] {
        assert!(
            matches!(parse_url(url), Err(UrlError::Malformed(_))),
            "userinfo should be refused: {url}"
        );
    }
}

/// A host carrying CR/LF can inject headers into whatever speaks the wire
/// protocol downstream. Refuse the whole URL rather than trusting every
/// consumer to re-sanitise.
#[test]
fn control_characters_in_url_are_refused() {
    for url in [
        "http://example.com\r\nX-Injected: 1/",
        "http://exam\tple.com/",
        "http://example.com\u{0}/",
        "http://exa\nmple.com/",
        "http://example .com/",
    ] {
        assert!(
            parse_url(url).is_err(),
            "control characters should be refused: {url:?}"
        );
    }
}

/// Non-ASCII hostnames are a homograph problem: `аpple.com` (Cyrillic а) is a
/// different host that reads identically. The allowlist is ASCII, so accepting
/// these can only ever fail closed — but failing closed with a clear error
/// beats failing closed with "could not resolve".
#[test]
fn non_ascii_hosts_are_refused() {
    for url in ["http://exаmple.com/", "http://例え.jp/"] {
        assert!(
            matches!(parse_url(url), Err(UrlError::Malformed(_))),
            "non-ascii host should be refused: {url}"
        );
    }
}

/// Only http/https reach the network. `file://` in particular turns an SSRF
/// into an arbitrary file read.
#[test]
fn non_http_schemes_are_refused() {
    for url in [
        "file:///etc/passwd",
        "gopher://127.0.0.1:6379/_INFO",
        "dict://127.0.0.1:11211/stat",
        "ftp://example.com/x",
        "ldap://127.0.0.1/",
        "jar:http://example.com!/",
    ] {
        assert!(
            matches!(
                parse_url(url),
                Err(UrlError::Scheme(_)) | Err(UrlError::Malformed(_))
            ),
            "scheme should be refused: {url}"
        );
    }
}

/// Malformed ports must fail, not silently fall back to 80 (which would let
/// `example.com:` reach a different service than the operator reviewed).
#[test]
fn malformed_ports_are_refused() {
    for url in [
        "http://example.com:99999/",
        "http://example.com:/",
        "http://example.com:-1/",
        "http://example.com:80x/",
        "http://[::1]:99999/",
    ] {
        assert!(
            matches!(parse_url(url), Err(UrlError::Malformed(_))),
            "bad port should be refused: {url}"
        );
    }
}

/// The allowlist is the first gate; none of the usual case/punctuation tricks
/// may widen it.
#[test]
fn allowlist_cannot_be_widened() {
    let p = policy_allowing(&["example.com", ".corp.internal"]);
    for host in [
        "evil.com",
        "notexample.com",
        "example.com.evil.com",
        "corp.internal.evil.com",
        "xexample.com",
    ] {
        assert!(!p.host_allowed(host), "must not be allowed: {host}");
    }
    // Case and a fully-qualified trailing dot name the same host.
    assert!(p.host_allowed("example.com"));
    assert!(p.host_allowed("api.corp.internal"));
    assert_eq!(
        parse_url("http://EXAMPLE.COM/").unwrap().host,
        "example.com"
    );
    assert_eq!(
        parse_url("http://example.com./").unwrap().host,
        "example.com"
    );
}

/// With nothing allowlisted the engine reaches nothing — it does not fall open
/// to "the whole internet minus a blocklist".
#[test]
fn closed_by_default() {
    let p = NetPolicy::default();
    for url in ["https://example.com/", "http://127.0.0.1/", "http://[::1]/"] {
        assert!(
            matches!(p.check(url), Err(UrlError::HostNotAllowed(_))),
            "must be refused with an empty allowlist: {url}"
        );
    }
    assert!(p.check_host("example.com").is_err());
}

/// `ping`/`traceroute`/`tcp_connect` take a bare host, so they need the same
/// treatment as a URL — a probe to the metadata address is still a reach-out.
#[test]
fn bare_host_probes_get_the_same_address_check() {
    let p = policy_allowing(&["localhost"]);
    assert!(
        p.check_host("localhost").is_err(),
        "localhost resolves to loopback and must be refused"
    );
    let unlisted = policy_allowing(&["example.com"]);
    assert!(unlisted.check_host("169.254.169.254").is_err());
}

/// The escape hatch exists, is off by default, and works when set — a sandboxed
/// deployment that genuinely wants loopback should not have to patch the code.
#[test]
fn private_access_requires_explicit_opt_in() {
    let mut p = policy_allowing(&["localhost"]);
    assert!(p.check("http://localhost/").is_err());
    p.allow_private = true;
    assert!(p.check("http://localhost/").is_ok());
}
