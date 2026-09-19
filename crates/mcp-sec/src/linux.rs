//! Linux backend for `secure_vault`: the freedesktop Secret Service (GNOME
//! Keyring, KWallet's bridge, KeePassXC), reached over the session bus.
//!
//! The existence check deliberately does **not** use `secret-tool search`:
//! that command prints the secret of every unlocked match, so the value would
//! pass through this process even if it were dropped on the floor. Instead
//! `SearchItems` on the service returns item paths only, and the item's
//! `Attributes` property carries the account. The secret stays in the daemon.
//!
//! Storing goes through `secret-tool store`, which reads the value on stdin
//! rather than argv, so it never shows in a process listing.

use std::io::Write;
use std::path::Path;

use mcp_types::{Envelope, ErrorCode};
use serde_json::{json, Value};

use crate::{SecModule, REDACTED};

const BUS_NAME: &str = "org.freedesktop.secrets";
const SERVICE_PATH: &str = "/org/freedesktop/secrets";

/// Where a tool this engine shells out to is expected to live. Absolute paths
/// keep an agent-controlled `PATH` out of the picture; `/usr/bin` first because
/// merged-usr distributions put everything there, `/bin` for the ones that do
/// not.
pub(crate) fn linux_tool(name: &str) -> Result<String, String> {
    for dir in ["/usr/bin", "/bin"] {
        let p = Path::new(dir).join(name);
        if p.is_file() {
            return Ok(p.display().to_string());
        }
    }
    Err(format!(
        "{name} is not installed (looked in /usr/bin and /bin)"
    ))
}

/// A string as a GVariant text-format literal: double quotes, C escapes.
pub(crate) fn gvariant_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Every quoted string in a `gdbus` reply, unescaped. GVariant prints strings
/// in single quotes, switching to double quotes when the value contains one.
fn quoted_strings(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\'' && c != '"' {
            continue;
        }
        let quote = c;
        let mut s = String::new();
        let mut closed = false;
        while let Some(n) = chars.next() {
            match n {
                '\\' => match chars.next() {
                    Some('n') => s.push('\n'),
                    Some('t') => s.push('\t'),
                    Some('r') => s.push('\r'),
                    Some('u') => {
                        let hex: String = chars.by_ref().take(4).collect();
                        if let Some(ch) =
                            u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)
                        {
                            s.push(ch);
                        }
                    }
                    Some(o) => s.push(o),
                    None => break,
                },
                n if n == quote => {
                    closed = true;
                    break;
                }
                n => s.push(n),
            }
        }
        if closed {
            out.push(s);
        }
    }
    out
}

/// Item object paths out of a `SearchItems` reply, unlocked and locked alike:
/// `([objectpath '/org/freedesktop/secrets/collection/login/28'], @ao [])`.
pub(crate) fn parse_object_paths(text: &str) -> Vec<String> {
    quoted_strings(text)
        .into_iter()
        .filter(|s| s.starts_with('/'))
        .collect()
}

/// One attribute out of a `Properties.Get ... Attributes` reply:
/// `(<{'account': 'alice', 'service': 'x'}>,)`.
pub(crate) fn parse_attribute(text: &str, key: &str) -> Option<String> {
    let strings = quoted_strings(text);
    strings
        .iter()
        .position(|s| s == key)
        .and_then(|i| strings.get(i + 1).cloned())
}

fn gdbus_call(gdbus: &str, object: &str, method: &str, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new(gdbus)
        .args([
            "call",
            "--session",
            "--dest",
            BUS_NAME,
            "--object-path",
            object,
            "--method",
            method,
        ])
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("gdbus: {e}"))?;
    if !out.status.success() {
        let e = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(format!(
            "the Secret Service did not answer over the session bus: {}",
            if e.is_empty() {
                "gdbus exited non-zero".to_string()
            } else {
                e
            }
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

impl SecModule {
    /// Does an item with these attributes exist? Paths only; the secret is
    /// never requested from the daemon.
    pub(crate) fn linux_vault_exists(&self, args: &Value) -> Envelope {
        let tool = "secure_vault";
        let Some(service) = args.get("service").and_then(Value::as_str) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'service'");
        };
        if !self.service_allowed(service) {
            return Envelope::fail(
                tool,
                ErrorCode::PolicyDenied,
                format!("service '{service}' is not in credentials.allowed_services"),
            );
        }
        let gdbus = match linux_tool("gdbus") {
            Ok(p) => p,
            Err(e) => {
                return Envelope::fail_with(
                    tool,
                    ErrorCode::ActionFailed,
                    format!("{e}; the Secret Service is reached through it"),
                    "install glib2's gdbus (package glib2 or libglib2.0-bin)",
                )
            }
        };
        let mut dict = format!("{{\"service\": {}", gvariant_string(service));
        if let Some(account) = args.get("account").and_then(Value::as_str) {
            dict.push_str(&format!(", \"account\": {}", gvariant_string(account)));
        }
        dict.push('}');
        let reply = match gdbus_call(
            &gdbus,
            SERVICE_PATH,
            "org.freedesktop.Secret.Service.SearchItems",
            &[&dict],
        ) {
            Ok(r) => r,
            Err(e) => return Envelope::fail(tool, ErrorCode::ActionFailed, e),
        };
        let paths = parse_object_paths(&reply);
        let Some(first) = paths.first() else {
            return Envelope::ok(tool, json!({ "service": service, "exists": false }));
        };
        // Attributes are metadata, not the secret; the account is the one a
        // caller needs to use the credential indirectly.
        let account = gdbus_call(
            &gdbus,
            first,
            "org.freedesktop.DBus.Properties.Get",
            &["org.freedesktop.Secret.Item", "Attributes"],
        )
        .ok()
        .and_then(|r| parse_attribute(&r, "account"));
        Envelope::ok(
            tool,
            json!({
                "service": service,
                "account": account,
                "exists": true,
                "matches": paths.len(),
                "value": REDACTED,
                "note": "secret values are never returned; use the credential indirectly",
            }),
        )
    }

    /// Store a secret through `secret-tool store`, the value on stdin.
    pub(crate) fn linux_vault_set(&self, args: &Value) -> Envelope {
        let tool = "secure_vault";
        let (Some(service), Some(account), Some(secret)) = (
            args.get("service").and_then(Value::as_str),
            args.get("account").and_then(Value::as_str),
            args.get("secret").and_then(Value::as_str),
        ) else {
            return Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                "'set' needs 'service', 'account' and 'secret'",
            );
        };
        if !self.service_allowed(service) {
            return Envelope::fail(
                tool,
                ErrorCode::PolicyDenied,
                format!("service '{service}' is not in credentials.allowed_services"),
            );
        }
        let secret_tool = match linux_tool("secret-tool") {
            Ok(p) => p,
            Err(e) => {
                return Envelope::fail_with(
                    tool,
                    ErrorCode::ActionFailed,
                    format!("{e}; the Secret Service is written through it"),
                    "install libsecret's secret-tool (package libsecret or libsecret-tools)",
                )
            }
        };
        let label = format!("{service} ({account})");
        let child = std::process::Command::new(&secret_tool)
            .arg("store")
            .arg(format!("--label={label}"))
            .args(["service", service, "account", account])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                return Envelope::fail(tool, ErrorCode::ActionFailed, format!("secret-tool: {e}"))
            }
        };
        if let Some(mut stdin) = child.stdin.take() {
            // A write failure surfaces as the tool's own exit status below.
            let _ = stdin.write_all(secret.as_bytes());
        }
        match child.wait_with_output() {
            Ok(o) if o.status.success() => Envelope::ok(
                tool,
                json!({ "service": service, "account": account, "stored": true }),
            ),
            Ok(o) => {
                let e = String::from_utf8_lossy(&o.stderr).trim().to_string();
                Envelope::fail(
                    tool,
                    ErrorCode::ActionFailed,
                    if e.is_empty() {
                        format!("secret-tool exited {}", o.status)
                    } else {
                        e.replace(secret, "***")
                    },
                )
            }
            Err(e) => Envelope::fail(tool, ErrorCode::ActionFailed, e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gvariant_strings_are_double_quoted_and_escaped() {
        assert_eq!(gvariant_string("plain"), "\"plain\"");
        assert_eq!(gvariant_string(""), "\"\"");
        assert_eq!(gvariant_string("a\"b"), "\"a\\\"b\"");
        assert_eq!(gvariant_string("a\\b"), "\"a\\\\b\"");
        assert_eq!(gvariant_string("line\nbreak"), "\"line\\nbreak\"");
        assert_eq!(gvariant_string("\u{1}"), "\"\\u0001\"");
        assert_eq!(gvariant_string("ünïcode"), "\"ünïcode\"");
    }

    #[test]
    fn object_paths_come_from_both_arrays() {
        assert_eq!(
            parse_object_paths(
                "([objectpath '/org/freedesktop/secrets/collection/login/28'], @ao [])\n"
            ),
            ["/org/freedesktop/secrets/collection/login/28"]
        );
        assert_eq!(
            parse_object_paths("([objectpath '/a/1', '/a/2'], [objectpath '/b/3'])"),
            ["/a/1", "/a/2", "/b/3"]
        );
        assert!(parse_object_paths("(@ao [], @ao [])\n").is_empty());
        assert!(parse_object_paths("").is_empty());
        assert!(parse_object_paths("Error: Could not connect").is_empty());
        // An unterminated quote yields nothing rather than a panic.
        assert!(parse_object_paths("(['/a/1").is_empty());
    }

    #[test]
    fn attributes_are_read_without_touching_the_secret() {
        let reply = "(<{'account': 'alice', 'agentctl-probe-attr': 'probe-1', 'xdg:schema': 'org.freedesktop.Secret.Generic'}>,)\n";
        assert_eq!(parse_attribute(reply, "account").as_deref(), Some("alice"));
        assert_eq!(
            parse_attribute(reply, "xdg:schema").as_deref(),
            Some("org.freedesktop.Secret.Generic")
        );
        assert_eq!(parse_attribute(reply, "service"), None);
        assert_eq!(parse_attribute("", "account"), None);
        assert_eq!(parse_attribute("(<@a{ss} {}>,)", "account"), None);
        // A value holding a single quote is printed in double quotes.
        assert_eq!(
            parse_attribute("(<{'account': \"o'brien\"}>,)", "account").as_deref(),
            Some("o'brien")
        );
        assert_eq!(
            parse_attribute("(<{'account': 'a\\'b'}>,)", "account").as_deref(),
            Some("a'b")
        );
    }

    #[test]
    fn linux_tool_names_the_missing_binary() {
        let e = linux_tool("definitely-not-a-real-tool-xyz").unwrap_err();
        assert!(e.contains("definitely-not-a-real-tool-xyz"), "{e}");
        assert!(linux_tool("sh").is_ok());
    }
}
