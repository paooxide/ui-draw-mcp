//! Mixed-Initiative 2FA/CAPTCHA Overlay & Handshake.
//!
//! Detects challenge states (Cloudflare Turnstile, Google reCAPTCHA, hCaptcha,
//! Arkose Labs, OTP/2FA inputs), injects a high-visibility, non-intrusive
//! browser HUD informing the human operator that action is required, and
//! resumes as soon as a 40ms clearance poll observes the challenge is gone.
//! The poll interval plus one probe round trip bounds detection latency; no
//! hard resume deadline is promised.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::backend::{BrowserBackend, BrowserError};

/// Kinds of detected verification challenges.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChallengeKind {
    CloudflareTurnstile,
    Recaptcha,
    Hcaptcha,
    Arkose,
    Otp2fa,
    Unknown,
}

impl ChallengeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChallengeKind::CloudflareTurnstile => "cloudflare_turnstile",
            ChallengeKind::Recaptcha => "recaptcha",
            ChallengeKind::Hcaptcha => "hcaptcha",
            ChallengeKind::Arkose => "arkose",
            ChallengeKind::Otp2fa => "otp_2fa",
            ChallengeKind::Unknown => "unknown",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            ChallengeKind::CloudflareTurnstile => "Cloudflare Turnstile",
            ChallengeKind::Recaptcha => "Google reCAPTCHA",
            ChallengeKind::Hcaptcha => "hCaptcha",
            ChallengeKind::Arkose => "Arkose Labs / FunCaptcha",
            ChallengeKind::Otp2fa => "2FA / One-Time Passcode",
            ChallengeKind::Unknown => "Verification Challenge",
        }
    }

    pub fn from_name(s: &str) -> Self {
        match s {
            "cloudflare_turnstile" => ChallengeKind::CloudflareTurnstile,
            "recaptcha" => ChallengeKind::Recaptcha,
            "hcaptcha" => ChallengeKind::Hcaptcha,
            "arkose" => ChallengeKind::Arkose,
            "otp_2fa" => ChallengeKind::Otp2fa,
            _ => ChallengeKind::Unknown,
        }
    }
}

impl std::str::FromStr for ChallengeKind {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::from_name(s))
    }
}

/// Status of challenge detection in a page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChallengeStatus {
    pub detected: bool,
    pub kind: Option<ChallengeKind>,
    pub details: String,
}

/// JavaScript probe to detect active, unsolved verification challenges.
pub const JS_DETECT_CHALLENGE: &str = r#"(function() {
  // 1. Cloudflare Turnstile
  var ts = document.querySelector('iframe[src*="cloudflare"], iframe[src*="turnstile"], .cf-turnstile, #cf-turnstile, .cf-turnstile-wrapper');
  if (ts) {
    var tsInput = document.querySelector('input[name="cf-turnstile-response"]');
    var tsSolved = tsInput && tsInput.value && tsInput.value.length > 10;
    if (!tsSolved) {
      return { detected: true, kind: "cloudflare_turnstile", details: "Cloudflare Turnstile challenge active" };
    }
  }

  // 2. Google reCAPTCHA
  var rc = document.querySelector('iframe[src*="recaptcha"], .g-recaptcha, #recaptcha, iframe[title*="reCAPTCHA"]');
  if (rc) {
    var rcInput = document.querySelector('textarea[name="g-recaptcha-response"], input[name="g-recaptcha-response"]');
    var rcSolved = rcInput && rcInput.value && rcInput.value.length > 10;
    if (!rcSolved) {
      return { detected: true, kind: "recaptcha", details: "Google reCAPTCHA challenge active" };
    }
  }

  // 3. hCaptcha
  var hc = document.querySelector('iframe[src*="hcaptcha"], .h-captcha, iframe[data-hcaptcha-widget-id]');
  if (hc) {
    var hcInput = document.querySelector('textarea[name="h-captcha-response"], input[name="h-captcha-response"]');
    var hcSolved = hcInput && hcInput.value && hcInput.value.length > 10;
    if (!hcSolved) {
      return { detected: true, kind: "hcaptcha", details: "hCaptcha challenge active" };
    }
  }

  // 4. Arkose Labs / FunCaptcha
  var arkose = document.querySelector('iframe[src*="arkoselabs"], iframe[src*="funcaptcha"], #fc-iframe-wrap');
  if (arkose) {
    return { detected: true, kind: "arkose", details: "Arkose Labs / FunCaptcha challenge active" };
  }

  // 5. 2FA / OTP inputs
  var otp = document.querySelector('input[autocomplete="one-time-code"], input[name*="otp" i], input[id*="otp" i], input[name*="2fa" i], input[id*="2fa" i], input[name*="passcode" i], input[name*="verification" i], input[placeholder*="verification code" i]');
  if (otp && (!otp.value || otp.value.length < 4)) {
    return { detected: true, kind: "otp_2fa", details: "One-Time Password / 2FA verification input detected" };
  }

  return { detected: false, kind: null, details: "no active challenge" };
})()"#;

/// Unwrap the `{"result": <value>}` envelope that `BrowserBackend::eval`
/// returns (both the CDP and Safari backends use it). A missing `result`
/// yields `Value::Null`.
pub fn eval_result(envelope: &Value) -> &Value {
    envelope.get("result").unwrap_or(&Value::Null)
}

/// Parse the detection probe's (already unwrapped) value. A value that is not
/// an object carrying a boolean `detected` is an error rather than "no
/// challenge", so a broken probe can never read as a cleared page.
fn parse_status(val: &Value) -> Result<ChallengeStatus, BrowserError> {
    let detected = val
        .get("detected")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            BrowserError::Failed(format!(
                "challenge probe returned an unexpected value: {val}"
            ))
        })?;
    let kind = val
        .get("kind")
        .and_then(Value::as_str)
        .map(ChallengeKind::from_name);
    let details = val
        .get("details")
        .and_then(Value::as_str)
        .unwrap_or("no details")
        .to_string();
    Ok(ChallengeStatus {
        detected,
        kind,
        details,
    })
}

/// Challenge manager providing mixed-initiative detection, HUD injection, and
/// a fast clearance poll with automatic resume.
pub struct ChallengeManager;

impl ChallengeManager {
    /// Detect whether an active challenge is present in the target tab.
    pub async fn detect(
        backend: &dyn BrowserBackend,
        target_id: &str,
    ) -> Result<ChallengeStatus, BrowserError> {
        let envelope = backend.eval(target_id, JS_DETECT_CHALLENGE).await?;
        parse_status(eval_result(&envelope))
    }

    /// Inject the mixed-initiative HUD into the page DOM.
    pub async fn inject_hud(
        backend: &dyn BrowserBackend,
        target_id: &str,
        kind: &ChallengeKind,
    ) -> Result<(), BrowserError> {
        let title = kind.display_name();
        let js = format!(
            r#"(function() {{
  var existing = document.getElementById('agentctl-challenge-hud');
  if (existing) return true;
  var hud = document.createElement('div');
  hud.id = 'agentctl-challenge-hud';
  hud.style.cssText = 'position:fixed; top:20px; left:50%; transform:translateX(-50%); z-index:2147483647; display:flex; align-items:center; gap:16px; background:rgba(15,23,42,0.95); backdrop-filter:blur(12px); border:2px solid #3b82f6; border-radius:12px; padding:14px 24px; color:#f8fafc; font-family:system-ui,-apple-system,sans-serif; box-shadow:0 20px 25px -5px rgba(0,0,0,0.6); pointer-events:none; transition:all 0.2s ease;';
  hud.innerHTML = `
    <div style="display:flex; align-items:center; gap:8px;">
      <span style="display:inline-block; width:12px; height:12px; border-radius:50%; background:#eab308; box-shadow:0 0 10px #eab308; animation:pulse 1.5s infinite;"></span>
      <strong style="color:#60a5fa; font-size:14px; text-transform:uppercase; letter-spacing:0.05em;">agentctl Handshake</strong>
    </div>
    <div style="height:20px; width:1px; background:#475569;"></div>
    <div style="display:flex; flex-direction:column; gap:2px;">
      <span style="font-size:14px; font-weight:600; color:#f1f5f9;">Human Action Required: ${{{}}}</span>
      <span style="font-size:12px; color:#94a3b8;">Complete verification in browser. Agent resumes automatically once verification clears.</span>
    </div>
  `;
  document.body.appendChild(hud);
  return true;
}})()"#,
            serde_json::to_string(title).unwrap_or_else(|_| "\"Verification\"".into())
        );
        backend.eval(target_id, &js).await?;
        Ok(())
    }

    /// Remove the HUD from the page DOM.
    pub async fn remove_hud(
        backend: &dyn BrowserBackend,
        target_id: &str,
    ) -> Result<(), BrowserError> {
        let js = r#"(function() {
  var hud = document.getElementById('agentctl-challenge-hud');
  if (hud && hud.parentNode) {
    hud.parentNode.removeChild(hud);
  }
  return true;
})()"#;
        backend.eval(target_id, js).await?;
        Ok(())
    }

    /// Wait for challenge clearance, polling every 40ms.
    ///
    /// `hud_removal_ms` in the result is the time from the poll that first
    /// observed clearance until the HUD removal call returned. It excludes
    /// the poll interval and the probe itself, so it is not an end-to-end
    /// resume latency. A probe error (other than the initial one, which also
    /// propagates) removes the HUD and is returned as an error: it is never
    /// reported as clearance.
    pub async fn wait_for_clearance(
        backend: &dyn BrowserBackend,
        target_id: &str,
        timeout_ms: u64,
    ) -> Result<Value, BrowserError> {
        let start = Instant::now();
        let timeout = Duration::from_millis(timeout_ms.clamp(100, 300_000));

        // Initial check
        let initial_status = Self::detect(backend, target_id).await?;
        if !initial_status.detected {
            return Ok(json!({
                "detected": false,
                "cleared": true,
                "kind": null,
                "duration_ms": start.elapsed().as_millis() as u64,
            }));
        }

        let kind = initial_status.kind.unwrap_or(ChallengeKind::Unknown);

        // Show HUD
        let _ = Self::inject_hud(backend, target_id, &kind).await;

        let poll_interval = Duration::from_millis(40);

        while start.elapsed() < timeout {
            tokio::time::sleep(poll_interval).await;
            match Self::detect(backend, target_id).await {
                Ok(status) => {
                    if !status.detected {
                        let observed = Instant::now();
                        let _ = Self::remove_hud(backend, target_id).await;
                        let hud_removal_ms = observed.elapsed().as_millis() as u64;
                        return Ok(json!({
                            "detected": true,
                            "cleared": true,
                            "kind": kind.as_str(),
                            "hud_removal_ms": hud_removal_ms,
                            "duration_ms": start.elapsed().as_millis() as u64,
                        }));
                    }
                }
                Err(e) => {
                    // Could not verify clearance (page error, navigation,
                    // disconnect): surface it. Best-effort HUD cleanup.
                    let _ = Self::remove_hud(backend, target_id).await;
                    return Err(e);
                }
            }
        }

        // Timeout expired: clean up HUD and return error
        let _ = Self::remove_hud(backend, target_id).await;
        Err(BrowserError::Timeout(format!(
            "Challenge '{}' was not cleared within {}ms",
            kind.display_name(),
            timeout_ms
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{Locator, Shot};
    use crate::cdp::DialogPolicy;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct MockChallengeBrowser {
        polls: AtomicU32,
        clear_after_polls: u32,
        /// Probe call index (0-based) at which eval fails.
        error_at_poll: Option<u32>,
    }

    #[async_trait]
    impl BrowserBackend for MockChallengeBrowser {
        async fn connect(&self, _p: Option<u16>, _l: Option<Value>) -> Result<Value, BrowserError> {
            Ok(json!({ "browser_id": 1 }))
        }
        async fn disconnect(&self, _b: u32, _k: bool) -> Result<Value, BrowserError> {
            Ok(json!({ "ok": true }))
        }
        async fn tabs(
            &self,
            _b: u32,
            _a: &str,
            _t: Option<&str>,
            _u: Option<&str>,
        ) -> Result<Value, BrowserError> {
            Ok(json!({ "tabs": [{"target_id": "T"}] }))
        }
        async fn navigate(
            &self,
            _t: &str,
            _a: &str,
            _u: Option<&str>,
        ) -> Result<Value, BrowserError> {
            Ok(json!({ "ok": true }))
        }
        async fn snapshot(
            &self,
            _t: &str,
            _m: &str,
            _r: Option<&str>,
        ) -> Result<Value, BrowserError> {
            Ok(json!({ "nodes": [] }))
        }
        async fn query(
            &self,
            _t: &str,
            _b: &str,
            _q: &str,
            _a: bool,
        ) -> Result<Value, BrowserError> {
            Ok(json!({ "found": true }))
        }
        async fn act(
            &self,
            _t: &str,
            _l: Locator<'_>,
            _a: &str,
            _v: Option<&str>,
        ) -> Result<Value, BrowserError> {
            Ok(json!({ "ok": true }))
        }
        async fn wait(
            &self,
            _t: &str,
            c: &str,
            _a: Option<&str>,
            _m: u64,
        ) -> Result<Value, BrowserError> {
            Ok(json!({ "settled": true, "condition": c }))
        }
        async fn screenshot(&self, _t: &str, _r: Option<&str>) -> Result<Shot, BrowserError> {
            Ok(Shot {
                width: 1280,
                height: 800,
                base64: "dummy".into(),
            })
        }
        async fn eval(&self, _t: &str, expr: &str) -> Result<Value, BrowserError> {
            if expr == JS_DETECT_CHALLENGE {
                let p = self.polls.fetch_add(1, Ordering::SeqCst);
                if self.error_at_poll == Some(p) {
                    return Err(BrowserError::Failed(
                        "eval: execution context destroyed".into(),
                    ));
                }
                // Real backends wrap the page value as {"result": <value>}.
                if p >= self.clear_after_polls {
                    Ok(
                        json!({ "result": { "detected": false, "kind": null, "details": "cleared" } }),
                    )
                } else {
                    Ok(
                        json!({ "result": { "detected": true, "kind": "cloudflare_turnstile", "details": "turnstile active" } }),
                    )
                }
            } else {
                Ok(json!({ "result": true }))
            }
        }
        async fn network(
            &self,
            _t: &str,
            _a: &str,
            _f: Option<&str>,
            _h: Option<Value>,
            _d: Option<u64>,
        ) -> Result<Value, BrowserError> {
            Ok(json!({ "ok": true }))
        }
        async fn dialog(&self, _t: &str, _p: Option<DialogPolicy>) -> Result<Value, BrowserError> {
            Ok(json!({ "ok": true }))
        }
        async fn cookies(
            &self,
            _t: &str,
            _a: &str,
            _c: Option<Value>,
        ) -> Result<Value, BrowserError> {
            Ok(json!({ "cookies": [] }))
        }
        async fn capture(&self, _t: &str, _a: &str, _o: &Value) -> Result<Value, BrowserError> {
            Ok(json!({ "ok": true }))
        }
        async fn assert(&self, _t: &str, _s: &Value) -> Result<Value, BrowserError> {
            Ok(json!({ "passed": true }))
        }
        async fn set_viewport(
            &self,
            _t: &str,
            _w: u32,
            _h: u32,
            _m: bool,
            _s: f64,
        ) -> Result<Value, BrowserError> {
            Ok(json!({ "ok": true }))
        }
        async fn fill_form(
            &self,
            _t: &str,
            _f: &Value,
            _s: Option<&Value>,
        ) -> Result<Value, BrowserError> {
            Ok(json!({ "filled": 1 }))
        }
        async fn extract(
            &self,
            _t: &str,
            _s: &Value,
            _w: Option<&str>,
        ) -> Result<Value, BrowserError> {
            Ok(json!({ "data": {} }))
        }
        async fn profile_state(&self, _t: &str) -> Result<Value, BrowserError> {
            Ok(json!({ "url": "about:blank" }))
        }
        async fn profile_restore(&self, _t: &str, _s: &Value) -> Result<Value, BrowserError> {
            Ok(json!({ "restored": true }))
        }
    }

    #[tokio::test]
    async fn test_challenge_detect_and_resume() {
        let browser = MockChallengeBrowser {
            polls: AtomicU32::new(0),
            clear_after_polls: 3, // Cleared on 3rd poll
            error_at_poll: None,
        };

        let t0 = Instant::now();
        let res = ChallengeManager::wait_for_clearance(&browser, "T", 5000)
            .await
            .unwrap();
        let elapsed = t0.elapsed();

        assert_eq!(res["detected"], true);
        assert_eq!(res["cleared"], true);
        assert_eq!(res["kind"], "cloudflare_turnstile");
        assert!(res["hud_removal_ms"].as_u64().is_some());
        assert!(res.get("resume_latency_ms").is_none());
        // 3 polls at ~40ms = ~120-180ms total
        assert!(elapsed.as_millis() < 600);
    }

    #[tokio::test]
    async fn test_challenge_none_detected_instant_return() {
        let browser = MockChallengeBrowser {
            polls: AtomicU32::new(0),
            clear_after_polls: 0, // Cleared immediately
            error_at_poll: None,
        };

        let res = ChallengeManager::wait_for_clearance(&browser, "T", 5000)
            .await
            .unwrap();
        assert_eq!(res["detected"], false);
        assert_eq!(res["cleared"], true);
    }

    #[tokio::test]
    async fn test_detect_unwraps_real_eval_envelope() {
        let browser = MockChallengeBrowser {
            polls: AtomicU32::new(0),
            clear_after_polls: 100,
            error_at_poll: None,
        };
        let st = ChallengeManager::detect(&browser, "T").await.unwrap();
        assert!(st.detected, "wrapped detected:true must be seen");
        assert_eq!(st.kind, Some(ChallengeKind::CloudflareTurnstile));
    }

    #[tokio::test]
    async fn test_probe_error_surfaces_instead_of_clearing() {
        let browser = MockChallengeBrowser {
            polls: AtomicU32::new(0),
            clear_after_polls: 100,
            error_at_poll: Some(2),
        };
        let res = ChallengeManager::wait_for_clearance(&browser, "T", 5000).await;
        assert!(
            matches!(res, Err(BrowserError::Failed(_))),
            "probe error must not read as cleared: {res:?}"
        );
    }

    #[test]
    fn test_parse_status_rejects_unexpected_shape() {
        assert!(parse_status(&json!(null)).is_err());
        assert!(parse_status(&json!({ "result": { "detected": true } })).is_err());
        assert!(parse_status(&json!({ "detected": false })).is_ok());
    }

    #[test]
    fn test_eval_result_unwraps() {
        assert_eq!(eval_result(&json!({ "result": 5 })), &json!(5));
        assert_eq!(eval_result(&json!({})), &Value::Null);
    }
}
