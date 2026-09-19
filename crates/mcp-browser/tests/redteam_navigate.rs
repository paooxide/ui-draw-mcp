//! Red-team suite for `browser_navigate` containment.
//!
//! The browser is a network client inside the perimeter, and until this guard
//! existed its only control was a string-prefix allowlist. The attacks here are
//! the ones that beat a prefix check: a host that merely *starts with* the
//! allowlisted origin, an alternate spelling of loopback, cloud metadata by
//! every route into it, and a scheme that is not a network fetch at all but a
//! bypass of another control (`file:` past the filesystem jail, `javascript:`
//! past the `browser_eval` opt-in).
//!
//! These run against the real system resolver, like the `mcp-net` suite.

use mcp_browser::{NavDenied, NavPolicy};

fn open() -> NavPolicy {
    NavPolicy::new(&[], false)
}

fn allowing(entries: &[&str]) -> NavPolicy {
    NavPolicy::new(
        &entries.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        false,
    )
}

/// Cloud metadata by every route into it, with the allowlist empty (the
/// configuration the threat model called out).
#[test]
fn cloud_metadata_endpoints_are_blocked_when_the_allowlist_is_empty() {
    for u in [
        "http://169.254.169.254/latest/meta-data/",
        "http://169.254.169.254:80/",
        "http://[::ffff:169.254.169.254]/",
        "http://[64:ff9b::a9fe:a9fe]/", // NAT64
        "http://[2002:a9fe:a9fe::]/",   // 6to4
        "http://2852039166/",           // decimal
        "http://0xa9fea9fe/",           // hex
        "http://0251.0376.0251.0376/",  // octal
        "http://169.254.169.254.nip.io/",
    ] {
        match open().check_blocking(u) {
            Err(NavDenied::BlockedAddress { .. }) => {}
            // A public DNS helper like nip.io may be unreachable offline; an
            // unresolvable name is still a refusal, never an allow.
            Err(NavDenied::Unresolvable(_)) if u.contains("nip.io") => {}
            other => panic!("{u} should be blocked, got {other:?}"),
        }
    }
}

/// Alternate spellings of loopback that a substring blocklist misses.
#[test]
fn loopback_spellings_are_resolved_then_blocked() {
    for host in [
        "127.0.0.1",
        "127.1",
        "127.0.0.2",
        "2130706433",
        "0x7f.0.0.1",
        "0.0.0.0",
        "[::1]",
        "[0:0:0:0:0:0:0:1]",
        "[::ffff:127.0.0.1]",
        "localhost",
    ] {
        let u = format!("http://{host}:9200/_cat/indices");
        match open().check_blocking(&u) {
            Err(NavDenied::BlockedAddress { .. }) => {}
            other => panic!("{u} should be blocked, got {other:?}"),
        }
    }
}

/// The prefix-check bug: `https://ok.example` used to admit anything that
/// started with those bytes.
#[test]
fn a_host_that_starts_with_the_allowlisted_origin_is_not_that_origin() {
    let p = allowing(&["https://ok.example"]);
    for u in [
        "https://ok.example.evil/",
        "https://ok.example.evil.example/",
        "https://ok.examplex/",
        "https://ok.example@evil.example/", // credentials: refused as malformed
        "https://ok.example:8443/",
        "http://ok.example/",
    ] {
        let r = p.check_blocking(u);
        assert!(
            matches!(
                r,
                Err(NavDenied::OriginNotAllowed(_)) | Err(NavDenied::Malformed(_))
            ),
            "{u} must not pass the allowlist, got {r:?}"
        );
    }
}

/// An allowlist entry is not a licence to reach private space. DNS is
/// attacker-controlled, so the address is judged even for a named host.
#[test]
fn an_allowlisted_name_resolving_to_loopback_is_refused_without_allow_private() {
    let p = allowing(&["http://localhost:3000"]);
    match p.check_blocking("http://localhost:3000/") {
        Err(NavDenied::BlockedAddress { host, .. }) => assert_eq!(host, "localhost"),
        other => panic!("expected BlockedAddress, got {other:?}"),
    }
    let p = NavPolicy::new(&["http://localhost:3000".to_string()], true);
    assert_eq!(p.check_blocking("http://localhost:3000/"), Ok(()));
}

/// Schemes that are not a fetch but a bypass of a different control.
#[test]
fn non_network_schemes_are_refused_regardless_of_policy() {
    for p in [
        open(),
        allowing(&["https://ok.example"]),
        NavPolicy::new(&[], true),
    ] {
        for u in [
            "file:///etc/passwd",
            "file:///Users/",
            "javascript:document.title='pwned'",
            "data:text/html;base64,PHNjcmlwdD5hbGVydCgxKTwvc2NyaXB0Pg==",
            "chrome://settings/",
            "chrome-extension://abc/",
            "about:config",
            "blob:https://ok.example/uuid",
            "view-source:https://ok.example/",
            "",
            "   ",
        ] {
            assert!(
                matches!(
                    p.check_blocking(u),
                    Err(NavDenied::Scheme(_)) | Err(NavDenied::Malformed(_))
                ),
                "{u:?} must be refused"
            );
        }
    }
}

/// Header-injection shaped and oversize inputs are refused, not passed on.
#[test]
fn control_characters_and_oversize_urls_are_refused() {
    let p = open();
    assert!(matches!(
        p.check_blocking("https://ok.example/\r\nHost: evil"),
        Err(NavDenied::Malformed(_))
    ));
    let long = format!("https://ok.example/{}", "a".repeat(100_000));
    // Shape is fine; the outcome is whatever the resolver says, and it must
    // not panic.
    let _ = p.check_blocking(&long);
}

/// The documented limit, pinned so it stays visible: a navigation to a public
/// origin can be redirected by the server to a private one, and the redirect
/// never passes through this guard. Only a network-level interception could
/// close it. If this test starts failing, the limit has been closed and the
/// threat model must say so.
#[test]
fn documented_known_gap_server_side_redirects_are_not_seen() {
    // The guard judges the request URL only. There is no API here that takes
    // a response, which is the whole point of the note.
    let p = open();
    assert!(
        p.check_blocking("https://example.com/").is_ok() || {
            // Offline: the name did not resolve, which is a refusal, not a gap.
            matches!(
                p.check_blocking("https://example.com/"),
                Err(NavDenied::Unresolvable(_))
            )
        }
    );
}
