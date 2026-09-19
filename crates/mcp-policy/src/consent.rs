//! The human-approval channel.
//!
//! Before this existed, `Mode::Interactive` did not actually ask anybody: a
//! `NeedConsent` decision was returned to the *agent* as `CONSENT_REQUIRED`.
//! That reads as a security control but is not one — the safety story for every
//! dangerous tool is "a human approves", and no human was ever asked.
//!
//! A [`ConsentProvider`] is the out-of-band path to a person. It is deliberately
//! **not** reachable from a [`ToolModule`](mcp_types::ToolModule): engines
//! *describe* risk, the core *decides*, and only the core may ask. An agent
//! therefore cannot approve itself.
//!
//! Every provider is fail-closed: anything that is not an explicit approval —
//! timeout, dismissal, a broken channel — denies.

use std::sync::atomic::{AtomicUsize, Ordering};
// Only DialogConsent needs this, and DialogConsent is macOS-only. An
// unconditional import is an unused-import warning on every other platform,
// and CI builds with `-D warnings`.
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::time::Duration;

/// What the human is being asked to approve.
#[derive(Debug, Clone)]
pub struct ConsentRequest {
    pub tool: String,
    /// One line: what will happen if this is approved.
    pub summary: String,
    /// Redacted argument detail, if the caller has any worth showing.
    pub details: Option<String>,
    pub session_id: String,
}

/// The human's answer. There is no "maybe" — everything that is not
/// [`Approved`](ConsentOutcome::Approved) denies the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentOutcome {
    Approved,
    Denied,
    /// Nobody answered in time.
    TimedOut,
    /// No channel is available (autonomous/headless).
    Unavailable,
}

impl ConsentOutcome {
    pub fn approved(self) -> bool {
        matches!(self, ConsentOutcome::Approved)
    }
    pub fn reason(self) -> &'static str {
        match self {
            ConsentOutcome::Approved => "approved",
            ConsentOutcome::Denied => "the human declined this action",
            ConsentOutcome::TimedOut => "no answer before the consent timeout",
            ConsentOutcome::Unavailable => "no consent channel is available",
        }
    }
}

/// An out-of-band way to ask a person.
pub trait ConsentProvider: Send + Sync {
    fn request(&self, req: &ConsentRequest) -> ConsentOutcome;
    /// Short name for the audit log.
    fn kind(&self) -> &'static str;
}

/// No channel: everything is denied. The correct default for autonomous or
/// headless runs — it never silently allows.
pub struct NoConsent;

impl ConsentProvider for NoConsent {
    fn request(&self, _req: &ConsentRequest) -> ConsentOutcome {
        ConsentOutcome::Unavailable
    }
    fn kind(&self) -> &'static str {
        "none"
    }
}

/// Caps how many times a session may interrupt a human.
///
/// Consent fatigue is an attack: an agent that can raise unlimited dialogs
/// trains the person to click Allow. Past the cap every further request denies
/// without prompting.
pub struct PromptBudget {
    max: usize,
    used: AtomicUsize,
}

impl PromptBudget {
    pub fn new(max: usize) -> Self {
        PromptBudget {
            max,
            used: AtomicUsize::new(0),
        }
    }
    /// Returns `true` while a prompt is still permitted.
    pub fn take(&self) -> bool {
        if self.max == 0 {
            return false;
        }
        self.used.fetch_add(1, Ordering::SeqCst) < self.max
    }
    pub fn used(&self) -> usize {
        self.used.load(Ordering::SeqCst)
    }
}

/// A native macOS approval dialog, driven through `osascript`.
///
/// Real channel, no client support required. The prompt shows the tool and the
/// redacted argument summary so a person approves *this* action rather than a
/// vague "allow?". Default button is **Deny** and the timeout denies, so an
/// absent or hurried human fails safe.
#[cfg(target_os = "macos")]
pub struct DialogConsent {
    timeout: Duration,
}

#[cfg(target_os = "macos")]
impl DialogConsent {
    pub fn new(timeout: Duration) -> Self {
        DialogConsent { timeout }
    }
}

#[cfg(target_os = "macos")]
impl Default for DialogConsent {
    fn default() -> Self {
        Self::new(Duration::from_secs(60))
    }
}

/// Escape a string for embedding in an AppleScript double-quoted literal.
///
/// Without this a crafted tool argument could close the literal and inject
/// AppleScript into the very dialog meant to gate it. Public because every
/// engine that shells out to `osascript` with agent-supplied text needs it, and
/// each writing its own is how one of them gets it wrong.
pub fn applescript_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            // Control characters would break the one-line literal.
            '\n' | '\r' | '\t' => out.push(' '),
            c if (c as u32) < 0x20 => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

#[cfg(target_os = "macos")]
impl ConsentProvider for DialogConsent {
    fn request(&self, req: &ConsentRequest) -> ConsentOutcome {
        let secs = self.timeout.as_secs().max(5);
        let mut body = format!("Tool: {}\n\n{}", req.tool, req.summary);
        if let Some(d) = &req.details {
            // Keep the dialog readable; the audit log holds the full record.
            let d: String = d.chars().take(400).collect();
            body.push_str(&format!("\n\n{d}"));
        }
        body.push_str(&format!("\n\nSession: {}", req.session_id));

        let script = format!(
            "display dialog \"{}\" with title \"agentctl needs approval\" \
             buttons {{\"Deny\", \"Allow\"}} default button \"Deny\" \
             with icon caution giving up after {secs}",
            applescript_escape(&body)
        );
        let out = std::process::Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(script)
            .output();
        match out {
            Ok(o) if o.status.success() => {
                let s = String::from_utf8_lossy(&o.stdout);
                // `giving up after` adds `gave up:true` when it expired.
                if s.contains("gave up:true") {
                    ConsentOutcome::TimedOut
                } else if s.contains("button returned:Allow") {
                    ConsentOutcome::Approved
                } else {
                    ConsentOutcome::Denied
                }
            }
            // Non-zero status is a dismissed dialog (user pressed Escape).
            Ok(_) => ConsentOutcome::Denied,
            Err(_) => ConsentOutcome::Unavailable,
        }
    }
    fn kind(&self) -> &'static str {
        "macos-dialog"
    }
}

/// A native dialog on Linux: `zenity --question` with **Deny** as the
/// default (cancel) button and a timeout that denies. When zenity is not
/// installed, a desktop notification with Allow and Deny actions through
/// `notify-send`, which also denies on timeout or dismissal. Neither can be
/// answered by the agent: both are drawn by the desktop, out of band.
#[cfg(target_os = "linux")]
pub struct DialogConsent {
    timeout: Duration,
}

#[cfg(target_os = "linux")]
impl DialogConsent {
    pub fn new(timeout: Duration) -> Self {
        DialogConsent { timeout }
    }

    /// The text of the dialog, shared by both channels.
    fn body(req: &ConsentRequest) -> String {
        let mut body = format!("Tool: {}\n\n{}", req.tool, req.summary);
        if let Some(d) = &req.details {
            let d: String = d.chars().take(400).collect();
            body.push_str(&format!("\n\n{d}"));
        }
        body.push_str(&format!("\n\nSession: {}", req.session_id));
        // Pango markup would let a crafted argument restyle the dialog;
        // zenity accepts --no-markup, and the text is passed as one argv
        // element so nothing in it is interpreted by a shell.
        body
    }

    fn zenity(&self, body: &str) -> Option<ConsentOutcome> {
        let secs = self.timeout.as_secs().max(5);
        let out = std::process::Command::new("/usr/bin/zenity")
            .args([
                "--question",
                "--title=agentctl needs approval",
                "--no-markup",
                "--icon=dialog-warning",
                "--ok-label=Allow",
                "--cancel-label=Deny",
                "--default-cancel",
                &format!("--timeout={secs}"),
                "--width=420",
                "--text",
            ])
            .arg(body)
            .stdin(std::process::Stdio::null())
            .output()
            .ok()?;
        Some(zenity_outcome(out.status.code()))
    }

    fn notification(&self, body: &str) -> Option<ConsentOutcome> {
        let ms = self.timeout.as_millis().max(5000).to_string();
        let out = std::process::Command::new("/usr/bin/notify-send")
            .args([
                "--app-name=agentctl",
                "--urgency=critical",
                "--wait",
                "--action=deny=Deny",
                "--action=allow=Allow",
                "--expire-time",
                &ms,
                "agentctl needs approval",
            ])
            .arg(body)
            .stdin(std::process::Stdio::null())
            .output()
            .ok()?;
        Some(notify_send_outcome(
            out.status.success(),
            &String::from_utf8_lossy(&out.stdout),
        ))
    }
}

/// zenity exits 0 for the OK button, 1 for cancel or Escape, 5 on timeout,
/// and anything else for a failure to show at all.
#[cfg(target_os = "linux")]
pub fn zenity_outcome(code: Option<i32>) -> ConsentOutcome {
    match code {
        Some(0) => ConsentOutcome::Approved,
        Some(1) => ConsentOutcome::Denied,
        Some(5) => ConsentOutcome::TimedOut,
        _ => ConsentOutcome::Unavailable,
    }
}

/// `notify-send --wait --action` prints the chosen action's key on stdout,
/// or nothing when the notification expired or was dismissed.
#[cfg(target_os = "linux")]
pub fn notify_send_outcome(ran: bool, stdout: &str) -> ConsentOutcome {
    if !ran {
        return ConsentOutcome::Unavailable;
    }
    match stdout.trim() {
        "allow" => ConsentOutcome::Approved,
        "deny" => ConsentOutcome::Denied,
        "" => ConsentOutcome::TimedOut,
        _ => ConsentOutcome::Denied,
    }
}

#[cfg(target_os = "linux")]
impl Default for DialogConsent {
    fn default() -> Self {
        Self::new(Duration::from_secs(60))
    }
}

#[cfg(target_os = "linux")]
impl ConsentProvider for DialogConsent {
    fn request(&self, req: &ConsentRequest) -> ConsentOutcome {
        let body = Self::body(req);
        if std::path::Path::new("/usr/bin/zenity").exists() {
            if let Some(o) = self.zenity(&body) {
                return o;
            }
        }
        if std::path::Path::new("/usr/bin/notify-send").exists() {
            if let Some(o) = self.notification(&body) {
                return o;
            }
        }
        ConsentOutcome::Unavailable
    }
    fn kind(&self) -> &'static str {
        "linux-dialog"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_channel_never_approves() {
        let p = NoConsent;
        let r = ConsentRequest {
            tool: "exec".into(),
            summary: "run rm -rf /".into(),
            details: None,
            session_id: "s".into(),
        };
        assert_eq!(p.request(&r), ConsentOutcome::Unavailable);
        assert!(!p.request(&r).approved());
    }

    #[test]
    fn only_explicit_approval_counts() {
        assert!(ConsentOutcome::Approved.approved());
        for o in [
            ConsentOutcome::Denied,
            ConsentOutcome::TimedOut,
            ConsentOutcome::Unavailable,
        ] {
            assert!(!o.approved(), "{o:?} must not approve");
        }
    }

    #[test]
    fn prompt_budget_stops_consent_fatigue() {
        let b = PromptBudget::new(2);
        assert!(b.take());
        assert!(b.take());
        assert!(!b.take(), "third prompt must be refused");
        assert!(!b.take());
    }

    #[test]
    fn zero_budget_refuses_immediately() {
        let b = PromptBudget::new(0);
        assert!(!b.take());
    }

    /// The mapping from what the desktop tools say to a decision. Only the
    /// one explicit answer approves; every other exit is a denial.
    #[cfg(target_os = "linux")]
    #[test]
    fn zenity_and_notify_send_answers_fail_closed() {
        assert_eq!(zenity_outcome(Some(0)), ConsentOutcome::Approved);
        assert_eq!(zenity_outcome(Some(1)), ConsentOutcome::Denied);
        assert_eq!(zenity_outcome(Some(5)), ConsentOutcome::TimedOut);
        assert_eq!(zenity_outcome(Some(255)), ConsentOutcome::Unavailable);
        assert_eq!(zenity_outcome(None), ConsentOutcome::Unavailable);
        assert_eq!(
            notify_send_outcome(true, "allow\n"),
            ConsentOutcome::Approved
        );
        assert_eq!(notify_send_outcome(true, "deny"), ConsentOutcome::Denied);
        assert_eq!(notify_send_outcome(true, ""), ConsentOutcome::TimedOut);
        assert_eq!(notify_send_outcome(true, "garbage"), ConsentOutcome::Denied);
        assert_eq!(
            notify_send_outcome(false, "allow"),
            ConsentOutcome::Unavailable
        );
    }

    /// A crafted argument must not be able to add its own options: the body
    /// is one argv element after `--text`, so a leading dash is text.
    #[cfg(target_os = "linux")]
    #[test]
    fn dialog_body_is_data_not_options() {
        let r = ConsentRequest {
            tool: "exec".into(),
            summary: "--ok-label=Deny --cancel-label=Allow".into(),
            details: Some("x".repeat(1000)),
            session_id: "s".into(),
        };
        let body = DialogConsent::body(&r);
        assert!(body.starts_with("Tool: exec"));
        assert!(
            body.contains("--ok-label=Deny"),
            "the text is kept verbatim as data"
        );
        assert!(body.len() < 600, "details are truncated for the dialog");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn applescript_literals_cannot_be_escaped() {
        // A crafted argument must not be able to close the string literal and
        // append its own AppleScript.
        let evil = r#"x" & (do shell script "touch /tmp/pwned") & ""#;
        let esc = applescript_escape(evil);
        // Every quote in the output must be backslash-escaped, so none of them
        // can terminate the literal we embed it in.
        let bytes = esc.as_bytes();
        for (i, b) in bytes.iter().enumerate() {
            if *b == b'"' {
                assert!(i > 0 && bytes[i - 1] == b'\\', "bare quote at {i} in {esc}");
            }
        }
        assert!(esc.contains("\\\""), "expected escaped quotes: {esc}");
        assert_eq!(applescript_escape("a\nb\tc"), "a b c");
        assert_eq!(applescript_escape(r"back\slash"), r"back\\slash");
    }
}
