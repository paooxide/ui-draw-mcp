//! Shadow Observation & Macro Learning Mode ("Ghost Mode").
//!
//! Observes human operator interactions in a browser tab, debounces keystrokes
//! and rapid click bursts, strips noise, and synthesizes clean, deterministic
//! `browser_flow` definitions.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::backend::{BrowserBackend, BrowserError};
use crate::cdp::RecordDialogs;
use crate::flow::{Flow, FlowError, FlowStore};

/// Raw interaction event captured from DOM event listeners.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawInteractionEvent {
    pub kind: String, // "click", "input", "change", "keydown", "navigate", "dialog"
    pub tag: String,
    pub selector: String,
    pub text: Option<String>,
    pub value: Option<String>,
    pub key: Option<String>,
    pub timestamp_ms: u64,
    pub url: Option<String>,
    /// True when the field holds a secret (password, OTP, card data). The
    /// recorder page script never sends such a value; see `is_secret_field`.
    #[serde(default)]
    pub secret: bool,
    /// The element's `id` and `name` attributes, sent for secret fields only,
    /// to name the field's `secret_ref`.
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

/// Placeholder an older recorder wrote for a secret value. The recorder now
/// writes a `secret_ref` instead; replay still refuses the placeholder.
pub use crate::flow::SECRET_PLACEHOLDER;

/// A stable, safe name for a secret field's `secret_ref`: its `id`, else its
/// `name`, else the selector, lowercased, with every run of characters outside
/// `a-z0-9` collapsed to one `_`, trimmed of `_` and capped at 40 characters.
/// Falls back to `secret` when nothing usable is left. The name is also what an
/// operator exports as `AGENTCTL_SECRET_<NAME>`, so it stays a plain identifier.
pub fn secret_ref_name(id: Option<&str>, name: Option<&str>, selector: &str) -> String {
    [id, name, Some(selector)]
        .into_iter()
        .flatten()
        .map(sanitize_ref)
        .find(|s| !s.is_empty())
        .unwrap_or_else(|| "secret".to_string())
}

fn sanitize_ref(raw: &str) -> String {
    let mut out = String::new();
    let mut pending_sep = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_sep && !out.is_empty() {
                out.push('_');
            }
            pending_sep = false;
            out.push(ch.to_ascii_lowercase());
        } else {
            pending_sep = true;
        }
    }
    out.truncate(40);
    out.trim_end_matches('_').to_string()
}

/// Whether an input must not have its value recorded.
///
/// A field is secret when any of these holds:
/// * `type=password`;
/// * its `name` or `id` contains a secret word (see [`has_secret_word`]);
/// * an `autocomplete` token is `one-time-code`, `current-password`,
///   `new-password`, starts with `cc-`, or is itself a secret word.
///
/// The page script in `JS_RECORDER_INSTALL` applies the same rule (a live test
/// compares the two on a table of fields, so they cannot drift).
pub fn is_secret_field(input_type: &str, name: &str, id: &str, autocomplete: &str) -> bool {
    if input_type.trim().eq_ignore_ascii_case("password") {
        return true;
    }
    if has_secret_word(name) || has_secret_word(id) {
        return true;
    }
    autocomplete.split_whitespace().any(|tok| {
        let t = tok.to_ascii_lowercase();
        t == "one-time-code"
            || t == "current-password"
            || t == "new-password"
            || t.starts_with("cc-")
            || has_secret_word(&t)
    })
}

/// Split an attribute value into lowercase words: on anything that is not an
/// ASCII letter or digit, and between a lowercase letter or digit and a
/// capital (`userPin` -> `user`, `pin`). ASCII-only on purpose, so the page
/// script's `/([a-z0-9])([A-Z])/` split gives the same answer.
fn attr_words(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut prev_lower_or_digit = false;
    for ch in s.chars() {
        if !ch.is_ascii_alphanumeric() {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            prev_lower_or_digit = false;
            continue;
        }
        if ch.is_ascii_uppercase() && prev_lower_or_digit && !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
        }
        cur.push(ch.to_ascii_lowercase());
        prev_lower_or_digit = ch.is_ascii_lowercase() || ch.is_ascii_digit();
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

/// Whether an attribute value (a `name`, `id` or `autocomplete` token) names a
/// secret: a word that is `pin`, `otp`, `cvv`, `cvc`, `ssn`, `pwd` or `token`,
/// or one containing `password`, `passwd`, `secret` or `apikey` (so
/// `newPassword`, `confirmpassword`, `client_secret`), or the pair `api` `key`
/// (`api_key`, `api-key`, `apiKey`). Short words must match whole, so
/// `spinner` and `shipping` are not secrets.
pub fn has_secret_word(s: &str) -> bool {
    let words = attr_words(s);
    let whole = ["pin", "otp", "cvv", "cvc", "ssn", "pwd", "token"];
    let within = ["password", "passwd", "secret", "apikey"];
    if words
        .iter()
        .any(|w| whole.contains(&w.as_str()) || within.iter().any(|k| w.contains(k)))
    {
        return true;
    }
    words.windows(2).any(|p| p[0] == "api" && p[1] == "key")
}

/// Longest label kept for a text locator.
const MAX_LABEL_CHARS: usize = 32;

/// The text of a clicked element that may be kept for a text locator, or
/// `None`. Free text is only ever used to find a button or link by its label,
/// so only a `BUTTON` or `A` target (as `tagName` spells it) keeps any, and
/// only a short single-line label, which is all `choose_locator` can use. An
/// `input` or `textarea` yields its *value* as `innerText`, so those never
/// qualify, and `secret_context` (the element or an ancestor is a secret field
/// or marked `data-private`, worked out in the page) discards the text
/// outright. The page script applies the same rule before the text leaves the
/// page.
pub fn clickable_text(tag: &str, text: &str, secret_context: bool) -> Option<String> {
    if secret_context || !(tag == "BUTTON" || tag == "A") {
        return None;
    }
    let t = text.trim();
    if t.is_empty() || t.chars().count() > MAX_LABEL_CHARS || t.contains(['\n', '\r']) {
        return None;
    }
    Some(t.to_string())
}

/// How soon after a click or key press a `navigate` event counts as its result.
const NAV_CAUSE_MS: u64 = 5_000;

/// Synthesizes raw captured interactions into deterministic `Flow` step definitions.
pub struct MacroSynthesizer;

impl MacroSynthesizer {
    /// Convert raw DOM events into clean `Flow` steps.
    pub fn synthesize(events: &[RawInteractionEvent]) -> Vec<Value> {
        // Dialogs are set aside first (a `beforeunload` dialog sits between a
        // click and the navigation it caused, which must still be seen as
        // that click's navigation) and put back below.
        let mut core: Vec<RawInteractionEvent> = Vec::new();
        let mut dialogs: Vec<(usize, Value)> = Vec::new();
        for ev in events {
            if ev.kind == "dialog" {
                if let Some(step) = Self::dialog_step(ev) {
                    dialogs.push((core.len(), step));
                }
            } else {
                core.push(ev.clone());
            }
        }
        let events: &[RawInteractionEvent] = &core;
        // `starts[k]`: where in `steps` the interaction holding event `k`
        // began, to place a dialog before the step that raised it.
        let mut starts: Vec<usize> = Vec::with_capacity(events.len());
        let mut steps = Vec::new();
        let mut i = 0;
        // (selector, secret_ref) of the secret fields named so far.
        let mut secret_names: Vec<(String, String)> = Vec::new();

        while i < events.len() {
            let ev = &events[i];
            let (first, steps_before) = (i, steps.len());

            match ev.kind.as_str() {
                "navigate" => {
                    if let Some(ref url) = ev.url {
                        if !url.is_empty() && url != "about:blank" {
                            steps.push(json!({
                                "op": "navigate",
                                "action": "goto",
                                "url": url
                            }));
                            steps.push(Self::settle_step());
                        }
                    }
                    i += 1;
                }
                "click" => {
                    // Debounce rapid duplicate clicks on same selector within 250ms
                    let mut advance = 1;
                    while i + advance < events.len() {
                        let next = &events[i + advance];
                        if next.kind == "click"
                            && next.selector == ev.selector
                            && next.timestamp_ms.saturating_sub(ev.timestamp_ms) < 250
                        {
                            advance += 1;
                        } else {
                            break;
                        }
                    }

                    // Choose best locator representation
                    let (by, query) =
                        Self::choose_locator(&ev.selector, ev.text.as_deref(), &ev.tag);

                    steps.push(json!({
                        "op": "act",
                        "action": "click",
                        "by": by,
                        "query": query
                    }));

                    // A click that took the page to a new document is followed
                    // by that navigation: wait for it rather than record it as a
                    // separate `goto` (replay would load the page twice).
                    let last_ts = events[i + advance - 1].timestamp_ms;
                    if Self::navigation_follows(events, i + advance, last_ts) {
                        advance += 1;
                        steps.push(Self::navigation_step());
                    } else {
                        // If button or link, insert automatic dom_settled wait
                        let is_trigger = ev.tag == "BUTTON"
                            || ev.tag == "A"
                            || ev.selector.contains("button")
                            || ev.selector.contains("btn")
                            || ev.selector.contains("submit");
                        if is_trigger {
                            steps.push(Self::settle_step());
                        }
                    }

                    i += advance;
                }
                "input" | "change" => {
                    // Coalesce sequential input/change events on the same element
                    let mut last_val = ev.value.clone().unwrap_or_default();
                    let mut secret = ev.secret;
                    let target_sel = &ev.selector;
                    let mut advance = 1;

                    while i + advance < events.len() {
                        let next = &events[i + advance];
                        if (next.kind == "input" || next.kind == "change")
                            && &next.selector == target_sel
                        {
                            if let Some(ref v) = next.value {
                                last_val = v.clone();
                            }
                            secret |= next.secret;
                            advance += 1;
                        } else {
                            break;
                        }
                    }

                    // Check if followed by an Enter keydown
                    let mut press_enter = false;
                    let mut enter_ts = 0;
                    if i + advance < events.len() {
                        let next = &events[i + advance];
                        if next.kind == "keydown" && next.key.as_deref() == Some("Enter") {
                            press_enter = true;
                            enter_ts = next.timestamp_ms;
                            advance += 1;
                        }
                    }

                    if secret {
                        // Never store the plaintext. The step names the value
                        // (`secret_ref`); whoever runs the flow supplies it in
                        // memory through `secrets`. The same field keeps the
                        // same name, and two different fields never share one.
                        let sel = target_sel.to_string();
                        let secret_ref = match secret_names.iter().find(|(s, _)| *s == sel) {
                            Some((_, r)) => r.clone(),
                            None => {
                                let base =
                                    secret_ref_name(ev.id.as_deref(), ev.name.as_deref(), &sel);
                                let mut candidate = base.clone();
                                let mut n = 2;
                                while secret_names.iter().any(|(_, r)| *r == candidate) {
                                    candidate = format!("{base}_{n}");
                                    n += 1;
                                }
                                secret_names.push((sel, candidate.clone()));
                                candidate
                            }
                        };
                        steps.push(json!({
                            "op": "act",
                            "action": "type",
                            "by": "css",
                            "query": target_sel,
                            "secret": true,
                            "secret_ref": secret_ref
                        }));
                    } else {
                        steps.push(json!({
                            "op": "act",
                            "action": "type",
                            "by": "css",
                            "query": target_sel,
                            "value": last_val
                        }));
                    }

                    if press_enter {
                        steps.push(Self::press_step(target_sel, "Enter"));
                        // Enter submitted a form that loaded a new document?
                        if Self::navigation_follows(events, i + advance, enter_ts) {
                            advance += 1;
                            steps.push(Self::navigation_step());
                        } else {
                            steps.push(Self::settle_step());
                        }
                    }

                    i += advance;
                }
                "keydown" => {
                    let mut advance_nav = false;
                    if let Some(ref k) = ev.key {
                        // Enter on a button/link already produced a recorded click.
                        let activates = ev.tag == "BUTTON" || ev.tag == "A";
                        if (k == "Enter" && !activates) || k == "Escape" || k == "Tab" {
                            steps.push(Self::press_step(&ev.selector, k));
                        }
                        // A key that loaded a new document (Enter in a form, a
                        // link focused with Tab and activated) is waited for.
                        if (k == "Enter" || k == "Tab" || k == "Escape")
                            && Self::navigation_follows(events, i + 1, ev.timestamp_ms)
                        {
                            advance_nav = true;
                            steps.push(Self::navigation_step());
                        }
                    }
                    i += if advance_nav { 2 } else { 1 };
                }
                _ => {
                    i += 1;
                }
            }
            starts.resize(starts.len() + (i - first), steps_before);
        }

        // A dialog is answered by the tab's standing policy, so its step goes
        // before the step whose action raised it: the last click, typing or
        // key press before it (or the very start, for one the page raised
        // on its own). Inserted last-first so equal positions keep their order.
        for (seen, step) in dialogs.into_iter().rev() {
            let at = events[..seen]
                .iter()
                .rposition(|e| matches!(e.kind.as_str(), "click" | "input" | "change" | "keydown"))
                .map_or(0, |k| starts[k]);
            steps.insert(at, step);
        }

        steps
    }

    /// The `dialog` step for a recorded dialog, or `None` for an `alert`
    /// (it has one way out, so there is nothing to reproduce). `policy` is how
    /// the dialog ended; a `prompt`'s typed text is not kept, because it may
    /// be a secret, so replay accepts it with empty text.
    fn dialog_step(ev: &RawInteractionEvent) -> Option<Value> {
        let kind = ev.text.as_deref().unwrap_or("");
        if kind == "alert" {
            return None;
        }
        let policy = if ev.value.as_deref() == Some("accept") {
            "accept"
        } else {
            "dismiss"
        };
        Some(json!({ "op": "dialog", "policy": policy, "type": kind }))
    }

    /// Whether `events[idx]` is a navigation the interaction at `cause_ts`
    /// brought about: a `navigate` event within [`NAV_CAUSE_MS`] of it. One
    /// that came on its own (a typed URL, a reload) is a `goto` instead.
    fn navigation_follows(events: &[RawInteractionEvent], idx: usize, cause_ts: u64) -> bool {
        events.get(idx).is_some_and(|e| {
            e.kind == "navigate" && e.timestamp_ms.saturating_sub(cause_ts) <= NAV_CAUSE_MS
        })
    }

    /// A replayable wait for the next document (`run_step` reads `navigation`).
    fn navigation_step() -> Value {
        json!({ "op": "wait", "navigation": true })
    }

    /// A replayable settle step (`run_step` reads the `dom_settled` key).
    fn settle_step() -> Value {
        json!({ "op": "wait", "dom_settled": true })
    }

    /// A replayable key press (`browser_act` action `press`, a real CDP key event).
    fn press_step(selector: &str, key: &str) -> Value {
        json!({
            "op": "act",
            "action": "press",
            "by": "css",
            "query": selector,
            "value": key
        })
    }

    /// Select the most stable and human-readable locator (prefer semantic IDs/testids/text).
    fn choose_locator(selector: &str, text: Option<&str>, tag: &str) -> (&'static str, String) {
        // A button or link with a short clean label is found by that label.
        // `clickable_text` is the one rule for what text may be used, so an
        // event carrying text it would refuse (an input's value) still gets
        // the selector.
        if let Some(label) = text.and_then(|t| clickable_text(tag, t, false)) {
            return ("text", label);
        }

        ("css", selector.to_string())
    }
}

/// Client-side recorder script. Registered to run at the start of every new
/// document in the observed tab (`Page.addScriptToEvaluateOnNewDocument`) and
/// run once in the current page; see [`RecordManager::start`]. It reports each
/// event through the `__agentctl_rec` binding. Top frame only: a selector
/// inside an iframe would not resolve when the flow replays.
///
/// It runs in an isolated world, and the binding exists only there: the DOM
/// is shared, so the listeners see the person's clicks and typing, but page
/// script has no way to call the binding or read `window.__agentctl_recorder`.
pub const JS_RECORDER_INSTALL: &str = r#"(function() {
  try { if (window.self !== window.top) return { installed: false, url: '' }; } catch (e) { return { installed: false, url: '' }; }
  // True for the page the recording began on; a document created later is a
  // navigation and announces itself below.
  var initial = window.__agentctl_rec_initial === true;
  if (window.__agentctl_recorder && window.__agentctl_recorder.active) {
    return { installed: true, already_active: true, url: window.location.href };
  }

  window.__agentctl_recorder = { active: true };

  function getBestSelector(el) {
    if (!el || el.nodeType !== Node.ELEMENT_NODE) return 'body';
    if (el.getAttribute('data-testid')) return `[data-testid="${el.getAttribute('data-testid')}"]`;
    if (el.getAttribute('data-test-id')) return `[data-test-id="${el.getAttribute('data-test-id')}"]`;
    if (el.getAttribute('data-qa')) return `[data-qa="${el.getAttribute('data-qa')}"]`;
    if (el.id && !/^[0-9]/.test(el.id) && el.id.length < 40 && !/[a-f0-9]{8,}/i.test(el.id)) {
      return `#${CSS.escape(el.id)}`;
    }
    if (el.getAttribute('name')) return `${el.tagName.toLowerCase()}[name="${CSS.escape(el.getAttribute('name'))}"]`;
    if (el.getAttribute('placeholder')) return `${el.tagName.toLowerCase()}[placeholder="${CSS.escape(el.getAttribute('placeholder'))}"]`;
    if (el.getAttribute('aria-label')) return `[aria-label="${CSS.escape(el.getAttribute('aria-label'))}"]`;

    // Path fallback
    var path = [];
    var curr = el;
    while (curr && curr.nodeType === Node.ELEMENT_NODE && curr !== document.body) {
      var tag = curr.tagName.toLowerCase();
      if (curr.id && !/[a-f0-9]{8,}/i.test(curr.id)) {
        path.unshift(`#${CSS.escape(curr.id)}`);
        break;
      }
      var parent = curr.parentElement;
      if (parent) {
        var siblings = Array.from(parent.children).filter(c => c.tagName === curr.tagName);
        if (siblings.length > 1) {
          var index = siblings.indexOf(curr) + 1;
          tag += `:nth-of-type(${index})`;
        }
      }
      path.unshift(tag);
      curr = parent;
    }
    return path.join(' > ') || el.tagName.toLowerCase();
  }

  // Twin of `has_secret_word` in record.rs: ASCII words split on non-alphanumerics
  // and at lower/digit-to-capital boundaries.
  function hasSecretWord(s) {
    var words = String(s || '').replace(/([a-z0-9])([A-Z])/g, '$1 $2').toLowerCase().split(/[^a-z0-9]+/).filter(Boolean);
    var whole = ['pin', 'otp', 'cvv', 'cvc', 'ssn', 'pwd', 'token'];
    var within = ['password', 'passwd', 'secret', 'apikey'];
    var hit = words.some(function(w) {
      return whole.indexOf(w) >= 0 || within.some(function(k) { return w.indexOf(k) >= 0; });
    });
    if (hit) return true;
    for (var i = 0; i + 1 < words.length; i++) {
      if (words[i] === 'api' && words[i + 1] === 'key') return true;
    }
    return false;
  }

  // Twin of `is_secret_field` in record.rs.
  function isSecret(el) {
    if (!el || !el.getAttribute) return false;
    var t = String(el.type || el.getAttribute('type') || '').trim().toLowerCase();
    if (t === 'password') return true;
    if (hasSecretWord(el.getAttribute('name')) || hasSecretWord(el.getAttribute('id'))) return true;
    var ac = String(el.getAttribute('autocomplete') || '').toLowerCase().split(/\s+/);
    return ac.some(function(tok) {
      return tok === 'one-time-code' || tok === 'current-password' || tok === 'new-password' || tok.indexOf('cc-') === 0 || hasSecretWord(tok);
    });
  }

  // Twin of `clickable_text` in record.rs: the label of a clicked button or
  // link, only when neither it nor an ancestor is a secret field or marked
  // data-private. Anything else (an input or textarea reads back its value as
  // innerText) yields null, so the text never leaves the page.
  function clickText(el) {
    if (!el || (el.tagName !== 'BUTTON' && el.tagName !== 'A')) return null;
    for (var n = el; n && n.nodeType === Node.ELEMENT_NODE; n = n.parentElement) {
      if (n.hasAttribute('data-private') || isSecret(n)) return null;
    }
    var t = (el.innerText || '').trim();
    if (!t || Array.from(t).length > 32 || /[\r\n]/.test(t)) return null;
    return t;
  }

  // Visual Recording Badge (Ghost Mode). A new document is still empty when
  // this script runs at its start, so the badge waits for <body>.
  function addBadge() {
    if (!document.body || document.getElementById('agentctl-recorder-badge')) return;
    if (!window.__agentctl_recorder.active) return;
    var badge = document.createElement('div');
    badge.id = 'agentctl-recorder-badge';
    badge.style.cssText = 'position:fixed; top:12px; right:16px; z-index:2147483647; display:flex; align-items:center; gap:8px; background:rgba(220,38,38,0.92); backdrop-filter:blur(8px); border:1px solid #fca5a5; border-radius:20px; padding:6px 14px; color:#ffffff; font-family:system-ui,-apple-system,sans-serif; font-size:12px; font-weight:700; letter-spacing:0.04em; box-shadow:0 10px 15px -3px rgba(0,0,0,0.4); pointer-events:none; text-transform:uppercase;';
    badge.innerHTML = '<span style="width:8px; height:8px; border-radius:50%; background:#ffffff; box-shadow:0 0 8px #ffffff;"></span> GHOST RECORDER';
    document.body.appendChild(badge);
  }
  if (document.body) addBadge();
  else document.addEventListener('DOMContentLoaded', addBadge);

  // Hand an event to the Rust side the moment it happens. The binding outlives
  // navigations (it belongs to the recording session, not to this document),
  // so nothing is lost when the page unloads right after a click.
  function emit(ev) {
    if (typeof window.__agentctl_rec === 'function') {
      try { window.__agentctl_rec(JSON.stringify(ev)); } catch (e) {}
    }
  }

  // A document that was not there when recording started announces itself, so
  // a navigation (a link, a form post, a reload, a typed URL) becomes a step.
  if (!initial) {
    emit({ kind: 'navigate', tag: '', selector: '', url: window.location.href });
  }
  // Listeners are registered once per document and read the live recorder
  // object, so a stop/start cycle in the same page must not add a second set
  // (every event would be recorded twice).
  var firstInstall = !window.__agentctl_recorder_listening;
  window.__agentctl_recorder_listening = true;
  if (firstInstall) {
  window.addEventListener('pageshow', function(e) {
    // Back/forward restored this document from the cache: no new document ran.
    if (e.persisted && window.__agentctl_recorder.active) {
      emit({ kind: 'navigate', tag: '', selector: '', url: window.location.href });
    }
  });

  window.addEventListener('click', function(e) {
    if (!window.__agentctl_recorder.active) return;
    var target = e.target;
    emit({
      kind: 'click',
      tag: target.tagName,
      selector: getBestSelector(target),
      text: clickText(target),
      value: null,
      key: null,
      url: window.location.href
    });
  }, true);

  function emitValue(kind, e) {
    if (!window.__agentctl_recorder.active) return;
    var target = e.target;
    var secret = isSecret(target);
    emit({
      kind: kind,
      tag: target.tagName,
      selector: getBestSelector(target),
      text: null,
      value: secret ? null : (target.value || ''),
      secret: secret,
      id: secret ? (target.id || null) : null,
      name: secret ? (target.getAttribute('name') || null) : null,
      key: null,
      url: window.location.href
    });
  }
  window.addEventListener('input', function(e) { emitValue('input', e); }, true);
  window.addEventListener('change', function(e) { emitValue('change', e); }, true);

  window.addEventListener('keydown', function(e) {
    if (!window.__agentctl_recorder.active) return;
    if (e.key === 'Enter' || e.key === 'Escape' || e.key === 'Tab') {
      var target = e.target;
      emit({
        kind: 'keydown',
        tag: target ? target.tagName : 'BODY',
        selector: getBestSelector(target),
        text: null,
        value: null,
        key: e.key,
        url: window.location.href
      });
    }
  }, true);
  }

  return { installed: true, active: true, url: window.location.href };
})()"#;

/// Run in the page when recording stops: silence the listeners of the current
/// document and take the badge down. (New documents get nothing, because the
/// new-document script is unregistered.)
pub const JS_RECORDER_TEARDOWN: &str = r#"(function() {
  var badge = document.getElementById('agentctl-recorder-badge');
  if (badge && badge.parentNode) {
    badge.parentNode.removeChild(badge);
  }
  if (window.__agentctl_recorder) {
    window.__agentctl_recorder.active = false;
  }
  return true;
})()"#;

/// Name of the page function the recorder reports through.
pub const RECORDER_BINDING: &str = "__agentctl_rec";

/// Macro Recorder Manager.
pub struct RecordManager;

impl RecordManager {
    /// Start recording in the target tab.
    ///
    /// The recorder is registered to run at the start of every new document in
    /// the tab (so it re-attaches after a reload or navigation) and is run once
    /// in the page as it is now. Events are delivered to this process as they
    /// happen, not collected from the page afterwards.
    pub async fn start(
        backend: &dyn BrowserBackend,
        target_id: &str,
    ) -> Result<Value, BrowserError> {
        Self::start_with(backend, target_id, None).await
    }

    /// [`Self::start`] with a say over who answers the page's JavaScript
    /// dialogs: `Human` leaves them to the person at the window and records
    /// how they answered, `Accept`/`Dismiss` answer them. `None` is `Human`
    /// for a visible browser and the tab's dialog policy otherwise.
    pub async fn start_with(
        backend: &dyn BrowserBackend,
        target_id: &str,
        dialogs: Option<RecordDialogs>,
    ) -> Result<Value, BrowserError> {
        let current = format!("window.__agentctl_rec_initial = true; {JS_RECORDER_INSTALL}");
        let res = backend
            .observe_start(
                target_id,
                RECORDER_BINDING,
                JS_RECORDER_INSTALL,
                &current,
                dialogs,
            )
            .await?;
        Ok(json!({
            "recording": true,
            "target_id": target_id,
            "installed": res.get("installed").and_then(Value::as_bool).unwrap_or(false),
            "url": res.get("url").cloned().unwrap_or(Value::Null),
            "dialogs": res.get("dialogs").cloned().unwrap_or(Value::Null)
        }))
    }

    /// Check recording status in the target tab.
    pub async fn status(
        backend: &dyn BrowserBackend,
        target_id: &str,
    ) -> Result<Value, BrowserError> {
        backend.observe_status(target_id).await
    }

    /// Stop recording and return the raw events in order, starting with a
    /// `navigate` to the page recording began on (unless that was blank).
    pub async fn stop_raw(
        backend: &dyn BrowserBackend,
        target_id: &str,
    ) -> Result<Vec<RawInteractionEvent>, BrowserError> {
        let res = backend
            .observe_stop(target_id, JS_RECORDER_TEARDOWN)
            .await?;
        let mut events: Vec<RawInteractionEvent> = Vec::new();
        if let Some(url) = res
            .get("start_url")
            .and_then(Value::as_str)
            .filter(|u| !u.is_empty() && *u != "about:blank")
        {
            events.push(RawInteractionEvent {
                kind: "navigate".into(),
                tag: String::new(),
                selector: String::new(),
                text: None,
                value: None,
                key: None,
                timestamp_ms: 0,
                url: Some(url.to_string()),
                secret: false,
                id: None,
                name: None,
            });
        }
        let raw = res
            .get("events")
            .and_then(Value::as_array)
            .ok_or_else(|| BrowserError::Failed("recorder stop returned no events array".into()))?;
        for v in raw {
            let ev = serde_json::from_value::<RawInteractionEvent>(v.clone())
                .map_err(|e| BrowserError::Failed(format!("malformed recorder event: {e}")))?;
            events.push(ev);
        }
        Ok(events)
    }

    /// Stop recording, extract captured events, and synthesize deterministic `Flow` steps.
    pub async fn stop(
        backend: &dyn BrowserBackend,
        target_id: &str,
    ) -> Result<Vec<Value>, BrowserError> {
        let events = Self::stop_raw(backend, target_id).await?;
        Ok(MacroSynthesizer::synthesize(&events))
    }

    /// Stop recording and save synthesized flow into `FlowStore`.
    pub async fn stop_and_save(
        backend: &dyn BrowserBackend,
        flow_store: &FlowStore,
        target_id: &str,
        flow_name: &str,
    ) -> Result<Flow, FlowError> {
        let steps = Self::stop(backend, target_id)
            .await
            .map_err(|e| FlowError::Io(format!("{e:?}")))?;

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);

        flow_store.save(flow_name, steps, now_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_debounce_keystrokes_into_single_type_step() {
        let events = vec![
            RawInteractionEvent {
                kind: "input".into(),
                tag: "INPUT".into(),
                selector: "#username".into(),
                text: None,
                value: Some("u".into()),
                key: None,
                timestamp_ms: 100,
                url: None,
                secret: false,
                id: None,
                name: None,
            },
            RawInteractionEvent {
                kind: "input".into(),
                tag: "INPUT".into(),
                selector: "#username".into(),
                text: None,
                value: Some("us".into()),
                key: None,
                timestamp_ms: 150,
                url: None,
                secret: false,
                id: None,
                name: None,
            },
            RawInteractionEvent {
                kind: "input".into(),
                tag: "INPUT".into(),
                selector: "#username".into(),
                text: None,
                value: Some("user@domain.com".into()),
                key: None,
                timestamp_ms: 200,
                url: None,
                secret: false,
                id: None,
                name: None,
            },
            RawInteractionEvent {
                kind: "keydown".into(),
                tag: "INPUT".into(),
                selector: "#username".into(),
                text: None,
                value: None,
                key: Some("Enter".into()),
                timestamp_ms: 250,
                url: None,
                secret: false,
                id: None,
                name: None,
            },
        ];

        let steps = MacroSynthesizer::synthesize(&events);
        assert_eq!(steps.len(), 3); // type, press Enter, wait dom_settled
        assert_eq!(steps[0]["op"], "act");
        assert_eq!(steps[0]["action"], "type");
        assert_eq!(steps[0]["query"], "#username");
        assert_eq!(steps[0]["value"], "user@domain.com");

        assert_eq!(steps[1]["op"], "act");
        assert_eq!(steps[1]["action"], "press");
        assert_eq!(steps[1]["value"], "Enter");

        assert_eq!(steps[2]["op"], "wait");
        assert_eq!(steps[2]["dom_settled"], true);
    }

    #[test]
    fn test_deduplicate_rapid_click_bursts() {
        let events = vec![
            RawInteractionEvent {
                kind: "click".into(),
                tag: "BUTTON".into(),
                selector: "[data-testid=\"save-btn\"]".into(),
                text: Some("Save Changes".into()),
                value: None,
                key: None,
                timestamp_ms: 100,
                url: None,
                secret: false,
                id: None,
                name: None,
            },
            // Jitter / bounce click within 50ms
            RawInteractionEvent {
                kind: "click".into(),
                tag: "BUTTON".into(),
                selector: "[data-testid=\"save-btn\"]".into(),
                text: Some("Save Changes".into()),
                value: None,
                key: None,
                timestamp_ms: 140,
                url: None,
                secret: false,
                id: None,
                name: None,
            },
        ];

        let steps = MacroSynthesizer::synthesize(&events);
        // Only 1 click step + 1 dom_settled wait
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0]["op"], "act");
        assert_eq!(steps[0]["action"], "click");
        assert_eq!(steps[0]["by"], "text");
        assert_eq!(steps[0]["query"], "Save Changes");
        assert_eq!(steps[1]["op"], "wait");
    }

    #[test]
    fn test_is_secret_field_decision() {
        // (type, name, id, autocomplete)
        let secret = [
            ("password", "", "", ""),
            ("PASSWORD", "", "", ""),
            ("text", "", "", "one-time-code"),
            ("text", "", "", "current-password"),
            ("text", "", "", "new-password"),
            ("text", "", "", "cc-number"),
            ("text", "", "", "section-a cc-csc"),
            ("text", "password", "", ""),
            ("text", "user_passwd", "", ""),
            ("text", "newPassword", "", ""),
            ("text", "confirmpassword", "", ""),
            ("text", "", "PWD", ""),
            ("text", "pin", "", ""),
            ("text", "user-pin", "", ""),
            ("text", "userPIN", "", ""),
            ("tel", "otp", "", ""),
            ("text", "", "otp-code", ""),
            ("text", "cvv", "", ""),
            ("text", "card_cvc", "", ""),
            ("text", "ssn", "", ""),
            ("text", "client_secret", "", ""),
            ("text", "", "csrf-token", ""),
            ("text", "api_key", "", ""),
            ("text", "api-key", "", ""),
            ("text", "apiKey", "", ""),
            ("text", "", "APIKEY", ""),
            ("text", "x", "y", "pin"),
        ];
        for (ty, name, id, ac) in secret {
            assert!(is_secret_field(ty, name, id, ac), "{ty} {name} {id} {ac}");
        }
        let plain = [
            ("text", "username", "", "username"),
            ("email", "email", "", "email"),
            ("text", "", "", ""),
            ("text", "spinner", "", ""),
            ("text", "shipping", "", ""),
            ("text", "pinterest", "", ""),
            ("text", "tokenizer", "", ""),
            ("text", "topic", "", ""),
            ("text", "api", "", ""),
            ("text", "key", "", ""),
            ("text", "monkey", "", ""),
            ("search", "q", "search-box", "off"),
        ];
        for (ty, name, id, ac) in plain {
            assert!(!is_secret_field(ty, name, id, ac), "{ty} {name} {id} {ac}");
        }
    }

    #[test]
    fn test_clickable_text_decision() {
        // A short single-line button or link label is kept (trimmed).
        assert_eq!(
            clickable_text("BUTTON", "  Save changes ", false).as_deref(),
            Some("Save changes")
        );
        assert_eq!(
            clickable_text("A", "Sign in", false).as_deref(),
            Some("Sign in")
        );
        // Never from inputs/textareas (their innerText is the value), or
        // other tags that a text locator does not use.
        for tag in ["INPUT", "TEXTAREA", "SELECT", "DIV", "SPAN", "P"] {
            assert_eq!(clickable_text(tag, "hunter2", false), None, "{tag}");
        }
        // Never from a secret or data-private context, whatever the tag.
        assert_eq!(clickable_text("BUTTON", "Show", true), None);
        assert_eq!(clickable_text("A", "Reveal", true), None);
        // Capped to what a locator can use: long, multi-line, empty text drop.
        assert_eq!(clickable_text("BUTTON", &"x".repeat(33), false), None);
        assert!(clickable_text("BUTTON", &"x".repeat(32), false).is_some());
        assert_eq!(clickable_text("BUTTON", "a\nb", false), None);
        assert_eq!(clickable_text("BUTTON", "   ", false), None);
    }

    #[test]
    fn test_locator_ignores_text_that_is_not_a_label() {
        // An event that somehow carries an input's value as text still gets
        // a selector locator, never a text locator.
        assert_eq!(
            MacroSynthesizer::choose_locator("#note", Some("my diary"), "TEXTAREA"),
            ("css", "#note".to_string())
        );
        assert_eq!(
            MacroSynthesizer::choose_locator("#go", Some("Go"), "BUTTON"),
            ("text", "Go".to_string())
        );
    }

    #[test]
    fn test_secret_events_never_store_value() {
        let ev = RawInteractionEvent {
            kind: "input".into(),
            tag: "INPUT".into(),
            selector: "#pw".into(),
            text: None,
            value: Some("hunter2".into()),
            key: None,
            timestamp_ms: 1,
            url: None,
            secret: true,
            id: None,
            name: None,
        };
        // Even if a raw value slipped through, a secret event is redacted.
        let steps = MacroSynthesizer::synthesize(&[ev]);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0]["secret"], true);
        // The step names its value; it holds neither the value nor a
        // placeholder that someone would be tempted to overwrite.
        assert_eq!(steps[0]["secret_ref"], "pw");
        assert!(steps[0].get("value").is_none(), "{}", steps[0]);
        assert!(!steps[0].to_string().contains("hunter2"));
    }

    fn ev(kind: &str, tag: &str, sel: &str, ts: u64) -> RawInteractionEvent {
        RawInteractionEvent {
            kind: kind.into(),
            tag: tag.into(),
            selector: sel.into(),
            text: None,
            value: None,
            key: None,
            timestamp_ms: ts,
            url: None,
            secret: false,
            id: None,
            name: None,
        }
    }

    fn nav(url: &str, ts: u64) -> RawInteractionEvent {
        RawInteractionEvent {
            url: Some(url.into()),
            ..ev("navigate", "", "", ts)
        }
    }

    #[test]
    fn test_navigation_after_a_click_is_waited_for_not_replayed_as_goto() {
        let mut typed = ev("input", "INPUT", "#b-input", 1_300);
        typed.value = Some("hello".into());
        let events = vec![
            nav("http://x/a", 0),
            ev("click", "A", "#to-b", 1_000),
            nav("http://x/b", 1_100),
            typed,
        ];
        let steps = MacroSynthesizer::synthesize(&events);
        let ops: Vec<String> = steps
            .iter()
            .map(|s| {
                format!(
                    "{}:{}",
                    s["op"].as_str().unwrap(),
                    s["action"]
                        .as_str()
                        .or(s["url"].as_str())
                        .or_else(|| s["navigation"].as_bool().map(|_| "navigation"))
                        .or_else(|| s["dom_settled"].as_bool().map(|_| "dom_settled"))
                        .unwrap_or("")
                )
            })
            .collect();
        assert_eq!(
            ops,
            [
                "navigate:goto",
                "wait:dom_settled",
                "act:click",
                "wait:navigation",
                "act:type",
            ],
            "{steps:?}"
        );
        assert_eq!(steps[0]["url"], "http://x/a");
        assert_eq!(steps[4]["query"], "#b-input");
        assert_eq!(steps[4]["value"], "hello");
    }

    #[test]
    fn test_a_navigation_nobody_caused_is_a_goto() {
        // Far from any interaction (a typed URL, a reload): replay must goto.
        let events = vec![
            nav("http://x/a", 0),
            ev("click", "BUTTON", "#b", 100),
            nav("http://x/c", 100 + NAV_CAUSE_MS + 1),
        ];
        let steps = MacroSynthesizer::synthesize(&events);
        let gotos: Vec<&str> = steps
            .iter()
            .filter(|s| s["op"] == "navigate")
            .map(|s| s["url"].as_str().unwrap())
            .collect();
        assert_eq!(gotos, ["http://x/a", "http://x/c"]);
        assert!(steps.iter().all(|s| s.get("navigation").is_none()));
        // The click that did not navigate keeps its settle.
        assert_eq!(steps[2]["action"], "click");
        assert_eq!(steps[3]["dom_settled"], true);
    }

    #[test]
    fn test_enter_that_submits_a_form_waits_for_the_next_document() {
        let mut typed = ev("input", "INPUT", "#q", 100);
        typed.value = Some("rust".into());
        let mut enter = ev("keydown", "INPUT", "#q", 150);
        enter.key = Some("Enter".into());
        let steps = MacroSynthesizer::synthesize(&[typed, enter, nav("http://x/r", 400)]);
        assert_eq!(steps.len(), 3, "{steps:?}");
        assert_eq!(steps[1]["action"], "press");
        assert_eq!(steps[2]["navigation"], true);
        assert!(steps.iter().all(|s| s["op"] != "navigate"));
    }

    #[test]
    fn test_secret_ref_names_are_stable_safe_and_distinct() {
        // id first, then name, then the selector; a plain lowercase identifier.
        assert_eq!(secret_ref_name(Some("pw"), Some("x"), "#pw"), "pw");
        assert_eq!(
            secret_ref_name(None, Some("user[pin]"), "input"),
            "user_pin"
        );
        assert_eq!(
            secret_ref_name(Some(""), None, "[data-testid=\"Card-CVV\"]"),
            "data_testid_card_cvv"
        );
        assert_eq!(secret_ref_name(Some("Pass Word!"), None, "x"), "pass_word");
        assert_eq!(secret_ref_name(Some("!!!"), None, "###"), "secret");
        assert!(secret_ref_name(Some(&"a".repeat(80)), None, "x").len() <= 40);

        let ev = |sel: &str, id: &str| RawInteractionEvent {
            kind: "input".into(),
            tag: "INPUT".into(),
            selector: sel.into(),
            text: None,
            value: None,
            key: None,
            timestamp_ms: 1,
            url: None,
            secret: true,
            id: Some(id.into()),
            name: None,
        };
        // The same field typed twice keeps one name; a different field whose
        // name collides gets its own.
        let steps = MacroSynthesizer::synthesize(&[
            ev("#a", "password"),
            ev("#b", "password"),
            ev("#a", "password"),
        ]);
        let refs: Vec<&str> = steps
            .iter()
            .map(|s| s["secret_ref"].as_str().unwrap())
            .collect();
        assert_eq!(refs, ["password", "password_2", "password"]);
    }

    #[test]
    fn test_a_dialog_step_comes_before_the_action_that_raised_the_dialog() {
        let dialog = |kind: &str, answer: &str, ts: u64| RawInteractionEvent {
            text: Some(kind.into()),
            value: Some(answer.into()),
            ..ev("dialog", "", "", ts)
        };
        let events = vec![
            nav("http://x/a", 0),
            ev("click", "BUTTON", "#first", 1_000),
            dialog("confirm", "accept", 1_050),
            ev("click", "BUTTON", "#second", 2_000),
            dialog("confirm", "dismiss", 2_050),
        ];
        let steps = MacroSynthesizer::synthesize(&events);
        let ops: Vec<String> = steps
            .iter()
            .map(|s| match s["op"].as_str().unwrap() {
                "dialog" => format!("dialog:{}", s["policy"].as_str().unwrap()),
                "act" => format!("click:{}", s["query"].as_str().unwrap()),
                other => other.to_string(),
            })
            .collect();
        assert_eq!(
            ops,
            [
                "navigate",
                "wait",
                "dialog:accept",
                "click:#first",
                "wait",
                "dialog:dismiss",
                "click:#second",
                "wait",
            ],
            "{steps:?}"
        );
        assert_eq!(steps[2]["type"], "confirm");
    }

    #[test]
    fn test_alerts_are_not_recorded_and_a_prompt_keeps_no_typed_text() {
        let mut alert = ev("dialog", "", "", 1_050);
        alert.text = Some("alert".into());
        alert.value = Some("accept".into());
        let mut prompt = ev("dialog", "", "", 2_050);
        prompt.text = Some("prompt".into());
        prompt.value = Some("accept".into());
        // Even if a stray field carried the typed answer, it is never copied.
        prompt.key = Some("hunter2".into());
        let events = vec![
            ev("click", "BUTTON", "#a", 1_000),
            alert,
            ev("click", "BUTTON", "#b", 2_000),
            prompt,
        ];
        let steps = MacroSynthesizer::synthesize(&events);
        let dialogs: Vec<&Value> = steps.iter().filter(|s| s["op"] == "dialog").collect();
        assert_eq!(
            dialogs.len(),
            1,
            "the alert has nothing to reproduce: {steps:?}"
        );
        assert_eq!(dialogs[0]["type"], "prompt");
        assert!(!serde_json::to_string(&steps).unwrap().contains("hunter2"));
    }

    #[test]
    fn test_a_beforeunload_dialog_does_not_turn_the_navigation_into_a_goto() {
        let events = vec![
            nav("http://x/a", 0),
            ev("click", "A", "#leave", 1_000),
            RawInteractionEvent {
                text: Some("beforeunload".into()),
                value: Some("accept".into()),
                ..ev("dialog", "", "", 1_050)
            },
            nav("http://x/b", 1_200),
        ];
        let steps = MacroSynthesizer::synthesize(&events);
        let gotos = steps
            .iter()
            .filter(|s| s["op"] == "navigate" && s["action"] == "goto")
            .count();
        assert_eq!(gotos, 1, "only the starting page is a goto: {steps:?}");
        assert!(steps
            .iter()
            .any(|s| s["op"] == "wait" && s["navigation"] == true));
        assert_eq!(steps[2]["op"], "dialog", "{steps:?}");
    }

    #[test]
    fn test_synthesized_steps_use_replayable_vocabulary() {
        let steps = MacroSynthesizer::synthesize(&[RawInteractionEvent {
            kind: "keydown".into(),
            tag: "INPUT".into(),
            selector: "#q".into(),
            text: None,
            value: None,
            key: Some("Escape".into()),
            timestamp_ms: 1,
            url: None,
            secret: false,
            id: None,
            name: None,
        }]);
        assert_eq!(steps[0]["action"], "press");
        assert_eq!(steps[0]["value"], "Escape");
    }
}
