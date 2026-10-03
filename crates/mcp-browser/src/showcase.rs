//! Browser Showcase & Visual Flair Engine: animated virtual SVG cursor,
//! smooth CSS transitions, expanding click ripple shockwaves, and floating
//! typing HUD badges for presentations, demos, and marketing recordings.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Showcase animation speed preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShowcaseSpeed {
    /// Cinematic: ~350ms glide, prominent ripple (video recording / marketing).
    Cinematic,
    /// Demo: ~220ms glide, crisp ripple (live presentation / demo).
    Demo,
    /// Snappy: ~120ms glide, subtle ripple (fast visual confirmation).
    Snappy,
    /// Off: 0ms glide, no overlays (standard headless CI execution).
    Off,
}

impl ShowcaseSpeed {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "cinematic" => Some(Self::Cinematic),
            "demo" | "presentation" => Some(Self::Demo),
            "snappy" | "fast" => Some(Self::Snappy),
            "off" | "none" | "instant" => Some(Self::Off),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Cinematic => "cinematic",
            Self::Demo => "demo",
            Self::Snappy => "snappy",
            Self::Off => "off",
        }
    }

    pub fn glide_ms(&self) -> u64 {
        match self {
            Self::Cinematic => 350,
            Self::Demo => 220,
            Self::Snappy => 120,
            Self::Off => 0,
        }
    }

    pub fn ripple_ms(&self) -> u64 {
        match self {
            Self::Cinematic => 500,
            Self::Demo => 400,
            Self::Snappy => 250,
            Self::Off => 0,
        }
    }
}

/// Visual style of the cursor pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorStyle {
    /// Modern gradient indigo arrow with drop shadow glow.
    GlowArrow,
    /// Cyberpunk neon cyan arrow with vibrant aura.
    NeonCyan,
    /// Compact luminous dot with outer pulse ring.
    MinimalDot,
}

impl CursorStyle {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "glow_arrow" | "glow" | "arrow" | "indigo" => Some(Self::GlowArrow),
            "neon_cyan" | "neon" | "cyan" => Some(Self::NeonCyan),
            "minimal_dot" | "dot" | "minimal" => Some(Self::MinimalDot),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::GlowArrow => "glow_arrow",
            Self::NeonCyan => "neon_cyan",
            Self::MinimalDot => "minimal_dot",
        }
    }
}

/// Complete configuration for browser showcase overlays and visual flair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShowcaseConfig {
    pub enabled: bool,
    pub speed: ShowcaseSpeed,
    pub click_ripple: bool,
    pub typing_hud: bool,
    pub cursor_style: CursorStyle,
    pub custom_glide_ms: Option<u64>,
}

impl Default for ShowcaseConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            speed: ShowcaseSpeed::Off,
            click_ripple: true,
            typing_hud: true,
            cursor_style: CursorStyle::GlowArrow,
            custom_glide_ms: None,
        }
    }
}

impl ShowcaseConfig {
    pub fn cinematic() -> Self {
        Self {
            enabled: true,
            speed: ShowcaseSpeed::Cinematic,
            click_ripple: true,
            typing_hud: true,
            cursor_style: CursorStyle::GlowArrow,
            custom_glide_ms: None,
        }
    }

    pub fn demo() -> Self {
        Self {
            enabled: true,
            speed: ShowcaseSpeed::Demo,
            click_ripple: true,
            typing_hud: true,
            cursor_style: CursorStyle::GlowArrow,
            custom_glide_ms: None,
        }
    }

    pub fn snappy() -> Self {
        Self {
            enabled: true,
            speed: ShowcaseSpeed::Snappy,
            click_ripple: true,
            typing_hud: true,
            cursor_style: CursorStyle::GlowArrow,
            custom_glide_ms: None,
        }
    }

    pub fn glide_ms(&self) -> u64 {
        if !self.enabled {
            return 0;
        }
        self.custom_glide_ms
            .unwrap_or_else(|| self.speed.glide_ms())
    }

    pub fn ripple_ms(&self) -> u64 {
        if !self.enabled || !self.click_ripple {
            return 0;
        }
        self.speed.ripple_ms()
    }

    pub fn to_json(&self) -> Value {
        json!({
            "enabled": self.enabled,
            "speed": self.speed.as_str(),
            "glide_ms": self.glide_ms(),
            "ripple_ms": self.ripple_ms(),
            "click_ripple": self.click_ripple,
            "typing_hud": self.typing_hud,
            "cursor_style": self.cursor_style.as_str()
        })
    }
}

/// Self-contained client-side JavaScript overlay injector that renders the animated SVG
/// virtual pointer, cubic-bezier glide transition, expanding click ripple shockwaves,
/// and floating action HUD badges.
pub const JS_SHOWCASE_ENGINE: &str = r####"
(function() {
  if (typeof window === 'undefined' || !document || !document.body) return;
  if (window.__agentctl_showcase_ready && document.getElementById('__agentctl_showcase_root')) return;
  window.__agentctl_showcase_ready = true;

  var existingStyle = document.getElementById('__agentctl_showcase_style');
  if (!existingStyle) {
    var style = document.createElement('style');
    style.id = '__agentctl_showcase_style';
    style.textContent = `
      #agentctl_showcase_cursor {
        position: fixed;
        top: 0;
        left: 0;
        width: 28px;
        height: 28px;
        pointer-events: none;
        z-index: 2147483647;
        transform: translate3d(-100px, -100px, 0);
        transition: transform var(--agentctl-glide-ms, 220ms) cubic-bezier(0.22, 1, 0.36, 1);
        filter: drop-shadow(0 3px 10px rgba(99, 102, 241, 0.65)) drop-shadow(0 0 3px rgba(255, 255, 255, 0.9));
        will-change: transform;
      }
      .agentctl_ripple_ring {
        position: fixed;
        border-radius: 50%;
        pointer-events: none;
        z-index: 2147483646;
        width: 44px;
        height: 44px;
        margin-left: -22px;
        margin-top: -22px;
        border: 2px solid #818cf8;
        box-shadow: 0 0 16px rgba(129, 140, 248, 0.8), inset 0 0 8px rgba(99, 102, 241, 0.5);
        animation: agentctl_ripple_anim var(--agentctl-ripple-ms, 400ms) cubic-bezier(0.1, 0.8, 0.3, 1) forwards;
      }
      @keyframes agentctl_ripple_anim {
        0% {
          transform: scale(0.15);
          opacity: 1;
        }
        100% {
          transform: scale(2.6);
          opacity: 0;
        }
      }
      #agentctl_showcase_hud {
        position: fixed;
        pointer-events: none;
        z-index: 2147483647;
        display: inline-flex;
        align-items: center;
        gap: 7px;
        padding: 5px 12px;
        border-radius: 9999px;
        background: rgba(15, 23, 42, 0.92);
        backdrop-filter: blur(12px);
        -webkit-backdrop-filter: blur(12px);
        border: 1px solid rgba(129, 140, 248, 0.45);
        box-shadow: 0 6px 20px rgba(0, 0, 0, 0.35), 0 0 12px rgba(99, 102, 241, 0.25);
        color: #f8fafc;
        font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
        font-size: 12px;
        font-weight: 500;
        opacity: 0;
        transform: translate3d(-100px, -100px, 0) scale(0.9);
        transition: opacity 180ms ease, transform 220ms cubic-bezier(0.22, 1, 0.36, 1);
        white-space: nowrap;
        max-width: 340px;
        overflow: hidden;
        text-overflow: ellipsis;
      }
      #agentctl_showcase_hud.agentctl_hud_active {
        opacity: 1;
        transform: translate3d(var(--agentctl-hud-x, 0px), var(--agentctl-hud-y, 0px), 0) scale(1);
      }
    `;
    document.head.appendChild(style);
  }

  var existingRoot = document.getElementById('__agentctl_showcase_root');
  if (!existingRoot) {
    var root = document.createElement('div');
    root.id = '__agentctl_showcase_root';
    root.innerHTML = `
      <div id="agentctl_showcase_cursor">
        <svg width="26" height="26" viewBox="0 0 24 24" fill="none" xmlns="http://www.w3.org/2000/svg">
          <defs>
            <linearGradient id="agentctl_c_grad" x1="0" y1="0" x2="24" y2="24" gradientUnits="userSpaceOnUse">
              <stop offset="0%" stop-color="#818cf8"/>
              <stop offset="100%" stop-color="#4f46e5"/>
            </linearGradient>
          </defs>
          <path d="M3 2L19 10L11 12L9 20L3 2Z" fill="url(#agentctl_c_grad)" stroke="#ffffff" stroke-width="1.5" stroke-linejoin="round"/>
        </svg>
      </div>
      <div id="agentctl_showcase_hud"></div>
    `;
    document.body.appendChild(root);
  }

  var hudTimer = null;
  function clearHud(h) {
    while (h.firstChild) h.removeChild(h.firstChild);
  }

  window.__agentctl_showcase = {
    move: function(x, y, glideMs) {
      var c = document.getElementById('agentctl_showcase_cursor');
      if (!c) return Promise.resolve();
      c.style.setProperty('--agentctl-glide-ms', (glideMs || 220) + 'ms');
      c.style.transform = 'translate3d(' + x + 'px, ' + y + 'px, 0)';
      return new Promise(function(resolve) {
        setTimeout(resolve, glideMs || 220);
      });
    },
    ripple: function(x, y, rippleMs) {
      var r = document.createElement('div');
      r.className = 'agentctl_ripple_ring';
      r.style.setProperty('--agentctl-ripple-ms', (rippleMs || 400) + 'ms');
      r.style.left = x + 'px';
      r.style.top = y + 'px';
      document.body.appendChild(r);
      setTimeout(function() {
        if (r.parentNode) r.parentNode.removeChild(r);
      }, (rippleMs || 400) + 50);
    },
    // True when what is being typed must never be put on screen: an explicit
    // `secret`, a password input, a one-time code, or a card field.
    sensitive: function(el, secret) {
      if (secret) return true;
      if (!el || !el.getAttribute) return false;
      try {
        var t = String(el.getAttribute('type') || '').toLowerCase();
        if (t === 'password') return true;
        var ac = String(el.getAttribute('autocomplete') || '').toLowerCase();
        if (ac.indexOf('password') >= 0 || ac.indexOf('one-time-code') >= 0 ||
            ac.indexOf('cc-') >= 0) return true;
      } catch (e) {}
      return false;
    },
    // Text goes in through textContent only: the value is agent-supplied and
    // this runs in the page's origin, so it must never be parsed as HTML.
    hud: function(x, y, text, icon) {
      var h = document.getElementById('agentctl_showcase_hud');
      if (!h) return;
      if (hudTimer) clearTimeout(hudTimer);
      clearHud(h);
      var i = document.createElement('span');
      i.textContent = icon || '⚡';
      var t = document.createElement('span');
      t.textContent = String(text == null ? '' : text);
      h.appendChild(i);
      h.appendChild(t);
      h.style.setProperty('--agentctl-hud-x', (x + 22) + 'px');
      h.style.setProperty('--agentctl-hud-y', (y + 12) + 'px');
      h.classList.add('agentctl_hud_active');
      hudTimer = setTimeout(function() {
        h.classList.remove('agentctl_hud_active');
        // Once faded, drop the text so it does not linger in innerText.
        hudTimer = setTimeout(function() { clearHud(h); }, 300);
      }, 1600);
    },
    destroy: function() {
      if (hudTimer) clearTimeout(hudTimer);
      hudTimer = null;
      var ids = ['__agentctl_showcase_root', '__agentctl_showcase_style'];
      for (var k = 0; k < ids.length; k++) {
        var n = document.getElementById(ids[k]);
        if (n && n.parentNode) n.parentNode.removeChild(n);
      }
      var rings = document.querySelectorAll('.agentctl_ripple_ring');
      for (var j = 0; j < rings.length; j++) {
        if (rings[j].parentNode) rings[j].parentNode.removeChild(rings[j]);
      }
      window.__agentctl_showcase_ready = false;
      try { delete window.__agentctl_showcase; } catch (e) { window.__agentctl_showcase = undefined; }
    },
    act: async function(x, y, action, value, glideMs, withRipple, withHud, el, secret) {
      if (withHud) {
        var label = action;
        var icon = '👆';
        if (action === 'type' || value) {
          var shown = this.sensitive(el, secret)
            ? '\u2022\u2022\u2022\u2022'
            : String(value == null ? '' : value).slice(0, 60);
          label = 'Type "' + shown + '"';
          icon = '⌨️';
        } else if (action === 'hover') {
          label = 'Hover';
          icon = '👀';
        } else if (action === 'click') {
          label = 'Click';
          icon = '🎯';
        }
        this.hud(x, y, label, icon);
      }
      if (glideMs > 0) {
        await this.move(x, y, glideMs);
      } else {
        var c = document.getElementById('agentctl_showcase_cursor');
        if (c) {
          c.style.transition = 'none';
          c.style.transform = 'translate3d(' + x + 'px, ' + y + 'px, 0)';
        }
      }
      if (withRipple && (action === 'click' || action === 'double_click')) {
        this.ripple(x, y, 400);
      }
    }
  };
})();
"####;

/// Removes everything [`JS_SHOWCASE_ENGINE`] put in a page. Safe to run on a
/// page that never had the overlay.
pub const JS_SHOWCASE_TEARDOWN: &str = r####"(function() {
  try {
    if (window.__agentctl_showcase && typeof window.__agentctl_showcase.destroy === 'function') {
      window.__agentctl_showcase.destroy();
    }
  } catch (e) {}
  var ids = ['__agentctl_showcase_root', '__agentctl_showcase_style'];
  for (var k = 0; k < ids.length; k++) {
    var n = document.getElementById(ids[k]);
    if (n && n.parentNode) n.parentNode.removeChild(n);
  }
  window.__agentctl_showcase_ready = false;
  return true;
})()"####;

#[cfg(test)]
mod tests {
    use super::*;

    /// The HUD must be built from text nodes. These assert the property on
    /// the shipped script; the live Chrome test proves it executes safely.
    #[test]
    fn hud_never_parses_agent_text_as_html() {
        // Only the static overlay template may use innerHTML.
        let uses: Vec<&str> = JS_SHOWCASE_ENGINE
            .lines()
            .filter(|l| l.contains("innerHTML"))
            .collect();
        assert_eq!(uses.len(), 1, "unexpected innerHTML use: {uses:?}");
        assert!(uses[0].contains("root.innerHTML"));
        assert!(JS_SHOWCASE_ENGINE.contains("t.textContent = String(text"));
    }

    #[test]
    fn test_showcase_speed_parsing() {
        assert_eq!(
            ShowcaseSpeed::parse("cinematic"),
            Some(ShowcaseSpeed::Cinematic)
        );
        assert_eq!(ShowcaseSpeed::parse("demo"), Some(ShowcaseSpeed::Demo));
        assert_eq!(ShowcaseSpeed::parse("snappy"), Some(ShowcaseSpeed::Snappy));
        assert_eq!(ShowcaseSpeed::parse("off"), Some(ShowcaseSpeed::Off));
        assert_eq!(ShowcaseSpeed::parse("instant"), Some(ShowcaseSpeed::Off));
        assert_eq!(ShowcaseSpeed::parse("unknown"), None);
    }

    #[test]
    fn test_showcase_presets_and_durations() {
        let cine = ShowcaseConfig::cinematic();
        assert!(cine.enabled);
        assert_eq!(cine.glide_ms(), 350);
        assert_eq!(cine.ripple_ms(), 500);

        let demo = ShowcaseConfig::demo();
        assert!(demo.enabled);
        assert_eq!(demo.glide_ms(), 220);
        assert_eq!(demo.ripple_ms(), 400);

        let snappy = ShowcaseConfig::snappy();
        assert!(snappy.enabled);
        assert_eq!(snappy.glide_ms(), 120);

        let off = ShowcaseConfig::default();
        assert!(!off.enabled);
        assert_eq!(off.glide_ms(), 0);
        assert_eq!(off.ripple_ms(), 0);
    }

    #[test]
    fn test_cursor_style_parsing() {
        assert_eq!(
            CursorStyle::parse("glow_arrow"),
            Some(CursorStyle::GlowArrow)
        );
        assert_eq!(CursorStyle::parse("neon_cyan"), Some(CursorStyle::NeonCyan));
        assert_eq!(
            CursorStyle::parse("minimal_dot"),
            Some(CursorStyle::MinimalDot)
        );
    }
}
