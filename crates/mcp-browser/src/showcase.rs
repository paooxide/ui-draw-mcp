//! Browser Showcase & Visual Flair Engine: animated virtual SVG cursor,
//! smooth CSS transitions, expanding click ripple shockwaves, and floating
//! typing HUD badges for presentations, demos, and marketing recordings.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Showcase animation speed preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShowcaseSpeed {
    /// Cinematic: ~350ms glide, long prominent ripple (video recording / marketing).
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
            // Long enough to land on several frames of a 30 fps screen
            // recording; the old 400-500 ms was almost never caught.
            Self::Cinematic => 900,
            Self::Demo => 700,
            Self::Snappy => 300,
            Self::Off => 0,
        }
    }
}

/// Visual style of the cursor pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorStyle {
    /// White arrow, dark outline, indigo glow: readable on light and dark pages.
    GlowArrow,
    /// Pale arrow with a cyan outline and a strong cyan neon aura.
    NeonCyan,
    /// Round white dot with a dark rim and a pulsing rose ring (no arrow).
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

/// Longest glide a caller may ask for. A glide is also a real wait inside the
/// action, so an unbounded value would hold every `browser_act` for as long.
pub const MAX_GLIDE_MS: u64 = 3000;
/// Cursor box size (px) when none is configured, and its bounds.
pub const DEFAULT_CURSOR_SIZE: u32 = 32;
pub const MIN_CURSOR_SIZE: u32 = 16;
pub const MAX_CURSOR_SIZE: u32 = 96;

/// A requested glide duration, held to `0..=MAX_GLIDE_MS`.
pub fn clamp_glide_ms(ms: u64) -> u64 {
    ms.min(MAX_GLIDE_MS)
}

/// A requested cursor size in px, held to the supported range.
pub fn clamp_cursor_size(px: u64) -> u32 {
    px.clamp(MIN_CURSOR_SIZE as u64, MAX_CURSOR_SIZE as u64) as u32
}

/// How long the click waits after the ripple starts, so the ring is already on
/// screen when the page reacts to the click.
pub fn ripple_beat_ms(ripple_ms: u64) -> u64 {
    (ripple_ms / 4).min(200)
}

fn default_cursor_size() -> u32 {
    DEFAULT_CURSOR_SIZE
}

/// Shape of a cursor style's pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    Arrow,
    Dot,
}

/// Everything the overlay script needs to draw one [`CursorStyle`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorVisual {
    pub shape: CursorShape,
    pub fill: &'static str,
    pub stroke: &'static str,
    pub stroke_width: &'static str,
    /// CSS `filter` (a stack of drop-shadows) giving the glow.
    pub glow: &'static str,
    /// Colour of the click ripple, the HUD border and the dot's pulse ring.
    pub accent: &'static str,
}

impl CursorStyle {
    pub fn visual(&self) -> CursorVisual {
        match self {
            Self::GlowArrow => CursorVisual {
                shape: CursorShape::Arrow,
                fill: "#ffffff",
                stroke: "#111827",
                stroke_width: "1.6",
                glow: "drop-shadow(0 0 4px rgba(99,102,241,0.95)) drop-shadow(0 0 12px rgba(99,102,241,0.75)) drop-shadow(0 2px 3px rgba(0,0,0,0.55))",
                accent: "#6366f1",
            },
            Self::NeonCyan => CursorVisual {
                shape: CursorShape::Arrow,
                fill: "#ecfeff",
                stroke: "#06b6d4",
                stroke_width: "2.2",
                glow: "drop-shadow(0 0 3px #22d3ee) drop-shadow(0 0 10px #22d3ee) drop-shadow(0 0 22px rgba(34,211,238,0.85))",
                accent: "#22d3ee",
            },
            Self::MinimalDot => CursorVisual {
                shape: CursorShape::Dot,
                fill: "#ffffff",
                stroke: "#0f172a",
                stroke_width: "1.6",
                glow: "drop-shadow(0 0 5px rgba(244,63,94,0.95)) drop-shadow(0 0 12px rgba(244,63,94,0.7)) drop-shadow(0 1px 2px rgba(0,0,0,0.5))",
                accent: "#f43f5e",
            },
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
    /// Cursor box size in px (`MIN_CURSOR_SIZE..=MAX_CURSOR_SIZE`).
    #[serde(default = "default_cursor_size")]
    pub cursor_size: u32,
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
            cursor_size: DEFAULT_CURSOR_SIZE,
        }
    }
}

impl ShowcaseConfig {
    fn preset(speed: ShowcaseSpeed) -> Self {
        Self {
            enabled: true,
            speed,
            ..Self::default()
        }
    }

    pub fn cinematic() -> Self {
        Self::preset(ShowcaseSpeed::Cinematic)
    }

    pub fn demo() -> Self {
        Self::preset(ShowcaseSpeed::Demo)
    }

    pub fn snappy() -> Self {
        Self::preset(ShowcaseSpeed::Snappy)
    }

    /// The overlay on at `speed` (`Off` leaves it disabled), with everything
    /// else at its default. What `demo = true` in the config turns on.
    pub fn for_speed(speed: ShowcaseSpeed) -> Self {
        Self {
            enabled: speed != ShowcaseSpeed::Off,
            speed,
            ..Self::default()
        }
    }

    pub fn glide_ms(&self) -> u64 {
        if !self.enabled {
            return 0;
        }
        clamp_glide_ms(
            self.custom_glide_ms
                .unwrap_or_else(|| self.speed.glide_ms()),
        )
    }

    pub fn ripple_ms(&self) -> u64 {
        if !self.enabled || !self.click_ripple {
            return 0;
        }
        self.speed.ripple_ms()
    }

    /// Apply the look-and-feel arguments of `browser_showcase`: `cursor_style`
    /// (an unknown name is ignored), `glide_ms` (capped) and `cursor_size`
    /// (clamped).
    pub fn apply_visual_args(&mut self, args: &Value) {
        if let Some(style) = args
            .get("cursor_style")
            .and_then(Value::as_str)
            .and_then(CursorStyle::parse)
        {
            self.cursor_style = style;
        }
        if let Some(ms) = args.get("glide_ms").and_then(Value::as_u64) {
            self.custom_glide_ms = Some(clamp_glide_ms(ms));
        }
        if let Some(px) = args.get("cursor_size").and_then(Value::as_u64) {
            self.cursor_size = clamp_cursor_size(px);
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "enabled": self.enabled,
            "speed": self.speed.as_str(),
            "glide_ms": self.glide_ms(),
            "ripple_ms": self.ripple_ms(),
            "click_ripple": self.click_ripple,
            "typing_hud": self.typing_hud,
            "cursor_style": self.cursor_style.as_str(),
            "cursor_size": self.cursor_size.clamp(MIN_CURSOR_SIZE, MAX_CURSOR_SIZE)
        })
    }

    /// The overlay script for this configuration (see [`JS_SHOWCASE_ENGINE`]).
    pub fn engine_js(&self) -> String {
        let v = self.cursor_style.visual();
        let cfg = json!({
            "style": self.cursor_style.as_str(),
            "size": self.cursor_size.clamp(MIN_CURSOR_SIZE, MAX_CURSOR_SIZE),
            "dot": v.shape == CursorShape::Dot,
            "fill": v.fill,
            "stroke": v.stroke,
            "strokeWidth": v.stroke_width,
            "glow": v.glow,
            "accent": v.accent,
        });
        JS_SHOWCASE_ENGINE.replace("__AGENTCTL_SHOWCASE_CONFIG__", &cfg.to_string())
    }
}

/// Steps of a glide: about one per frame at 60 fps, never more than this.
const MAX_GLIDE_STEPS: u64 = 40;

/// The eased progress (0..=1) of a glide at linear time `t`, the same curve
/// the on-page cursor uses (`cubic-bezier(0.22, 1, 0.36, 1)`), so the real
/// pointer and the drawn cursor travel together.
pub fn glide_ease(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    let (x1, y1, x2, y2) = (0.22, 1.0, 0.36, 1.0);
    let bez = |a: f64, b: f64, u: f64| {
        3.0 * (1.0 - u) * (1.0 - u) * u * a + 3.0 * (1.0 - u) * u * u * b + u * u * u
    };
    // Solve x(u) = t by bisection; x is monotonic for these control points.
    let (mut lo, mut hi) = (0.0f64, 1.0f64);
    for _ in 0..32 {
        let mid = (lo + hi) / 2.0;
        if bez(x1, x2, mid) < t {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    bez(y1, y2, (lo + hi) / 2.0)
}

/// How many `mouseMoved` events a glide of `glide_ms` is spread over.
pub fn glide_step_count(glide_ms: u64) -> usize {
    (glide_ms / 16).clamp(1, MAX_GLIDE_STEPS) as usize
}

/// The points of an eased glide from `from` to `to`, last one exactly `to`.
/// `glide_ms == 0` is a single jump.
pub fn glide_path(from: (f64, f64), to: (f64, f64), glide_ms: u64) -> Vec<(f64, f64)> {
    let n = if glide_ms == 0 {
        1
    } else {
        glide_step_count(glide_ms)
    };
    (1..=n)
        .map(|i| {
            if i == n {
                return to;
            }
            let e = glide_ease(i as f64 / n as f64);
            (from.0 + (to.0 - from.0) * e, from.1 + (to.1 - from.1) * e)
        })
        .collect()
}

/// Self-contained client-side JavaScript overlay injector that renders the
/// virtual pointer (per [`CursorStyle`]), its glide, the click ripple and the
/// floating action HUD. `__AGENTCTL_SHOWCASE_CONFIG__` is replaced by
/// [`ShowcaseConfig::engine_js`].
///
/// Everything is built with `createElement`/`createElementNS` and styled
/// through `element.style` and the Web Animations API: no `innerHTML` (Trusted
/// Types pages forbid it) and no `<style>` element (a strict `style-src` CSP
/// would not apply it). `window.__agentctl_showcase` and the ready flag are set
/// only once the DOM exists. The DOM is shared between worlds, so while a
/// recording is running the act script re-installs the API in the recorder's
/// isolated world and reuses the elements already on the page.
pub const JS_SHOWCASE_ENGINE: &str = r####"
(function() {
  if (typeof window === 'undefined' || !document || !document.body) return;
  var CFG = __AGENTCTL_SHOWCASE_CONFIG__;
  var key = CFG.style + ':' + CFG.size;
  var NS = 'http://www.w3.org/2000/svg';
  var root = document.getElementById('__agentctl_showcase_root');
  if (root && root.getAttribute('data-cfg') !== key) {
    if (root.parentNode) root.parentNode.removeChild(root);
    root = null;
  }
  if (window.__agentctl_showcase_ready && window.__agentctl_showcase && root) return;

  function css(n, o) { for (var k in o) n.style.setProperty(k, o[k]); return n; }
  function svg(tag, attrs) {
    var n = document.createElementNS(NS, tag);
    for (var k in attrs) n.setAttribute(k, String(attrs[k]));
    return n;
  }
  var S = CFG.size;
  var tip = CFG.dot ? S / 2 : S * 2 / 24;

  if (!root) {
    root = document.createElement('div');
    root.id = '__agentctl_showcase_root';
    root.setAttribute('data-cfg', key);
    css(root, { 'pointer-events': 'none' });

    var cur = document.createElement('div');
    cur.id = 'agentctl_showcase_cursor';
    css(cur, {
      position: 'fixed', top: '0', left: '0', width: S + 'px', height: S + 'px',
      'pointer-events': 'none', 'z-index': '2147483647',
      transform: 'translate3d(-300px, -300px, 0)', 'will-change': 'transform',
      filter: CFG.glow
    });
    var sv = svg('svg', { width: S, height: S, viewBox: '0 0 24 24', id: 'agentctl_showcase_cursor_svg' });
    css(sv, { display: 'block', overflow: 'visible', 'margin-left': (-tip) + 'px',
      'margin-top': (-tip) + 'px', 'transform-origin': tip + 'px ' + tip + 'px' });
    if (CFG.dot) {
      var ring = svg('circle', { cx: 12, cy: 12, r: 11, fill: 'none', stroke: CFG.accent, 'stroke-width': 2 });
      css(ring, { 'transform-box': 'fill-box', 'transform-origin': 'center' });
      var dot = svg('circle', { cx: 12, cy: 12, r: 6, fill: CFG.fill, stroke: CFG.stroke, 'stroke-width': CFG.strokeWidth });
      sv.appendChild(ring);
      sv.appendChild(dot);
      try {
        ring.animate([{ transform: 'scale(0.7)', opacity: 0.95 }, { transform: 'scale(1.12)', opacity: 0.15 }],
          { duration: 1100, iterations: Infinity, easing: 'ease-out' });
      } catch (e) {}
    } else {
      sv.appendChild(svg('path', {
        d: 'M2 2 L2 19.5 L6.6 15.2 L9.8 22 L13.2 20.5 L10 13.8 L16.4 13.8 Z',
        fill: CFG.fill, stroke: CFG.stroke, 'stroke-width': CFG.strokeWidth, 'stroke-linejoin': 'round'
      }));
    }
    cur.appendChild(sv);
    root.appendChild(cur);

    var hudEl = document.createElement('div');
    hudEl.id = 'agentctl_showcase_hud';
    css(hudEl, {
      position: 'fixed', left: '-400px', top: '-400px', 'pointer-events': 'none',
      'z-index': '2147483647', display: 'inline-flex', 'align-items': 'center', gap: '7px',
      padding: '5px 12px', 'border-radius': '9999px', background: 'rgba(15, 23, 42, 0.92)',
      border: '1px solid ' + CFG.accent, color: '#f8fafc',
      'box-shadow': '0 6px 20px rgba(0,0,0,0.35), 0 0 12px ' + CFG.accent,
      'font-family': '-apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif',
      'font-size': '12px', 'font-weight': '500', opacity: '0', 'white-space': 'nowrap',
      'max-width': '340px', overflow: 'hidden', 'text-overflow': 'ellipsis',
      transition: 'opacity 180ms ease'
    });
    root.appendChild(hudEl);
    document.body.appendChild(root);
  }

  var hudTimer = null;
  function clearHud(h) {
    while (h.firstChild) h.removeChild(h.firstChild);
  }
  function sleep(ms) { return new Promise(function(r) { setTimeout(r, ms); }); }
  function cursor() { return document.getElementById('agentctl_showcase_cursor'); }

  window.__agentctl_showcase = {
    // Jump the cursor without animating (also where a cursor resumes after a
    // navigation: the new document starts it off-screen).
    place: function(x, y) {
      var c = cursor();
      if (!c) return;
      c.style.setProperty('transition', 'none');
      c.style.setProperty('transform', 'translate3d(' + x + 'px, ' + y + 'px, 0)');
    },
    move: function(x, y, glideMs) {
      var c = cursor();
      if (!c) return Promise.resolve();
      if (!(glideMs > 0)) { this.place(x, y); return Promise.resolve(); }
      c.style.setProperty('transition', 'none');
      void c.getBoundingClientRect(); // commit the start before the transition is set
      c.style.setProperty('transition', 'transform ' + glideMs + 'ms cubic-bezier(0.22, 1, 0.36, 1)');
      c.style.setProperty('transform', 'translate3d(' + x + 'px, ' + y + 'px, 0)');
      return sleep(glideMs);
    },
    // Expanding double ring in the accent colour, lasting rippleMs.
    ripple: function(x, y, rippleMs) {
      var ms = rippleMs > 0 ? rippleMs : 700;
      var base = Math.max(64, S * 2);
      for (var i = 0; i < 2; i++) {
        var r = document.createElement('div');
        r.className = 'agentctl_ripple_ring';
        css(r, {
          position: 'fixed', left: x + 'px', top: y + 'px', width: base + 'px', height: base + 'px',
          'margin-left': (-base / 2) + 'px', 'margin-top': (-base / 2) + 'px', 'border-radius': '50%',
          'box-sizing': 'border-box', 'pointer-events': 'none', 'z-index': '2147483646',
          border: (i ? 3 : 5) + 'px solid ' + CFG.accent,
          background: i ? 'transparent' : CFG.accent + '33',
          'box-shadow': '0 0 28px ' + CFG.accent + ', inset 0 0 14px ' + CFG.accent
        });
        root.appendChild(r);
        (function(ring, second) {
          var done = function() { if (ring.parentNode) ring.parentNode.removeChild(ring); };
          try {
            var a = ring.animate(
              [{ transform: 'scale(0.2)', opacity: 1 }, { transform: 'scale(' + (second ? 3.4 : 2.5) + ')', opacity: 0 }],
              { duration: second ? ms * 0.85 : ms, delay: second ? ms * 0.15 : 0, easing: 'cubic-bezier(0.1, 0.8, 0.3, 1)', fill: 'both' });
            a.onfinish = done;
          } catch (e) {}
          setTimeout(done, ms + 300);
        })(r, i === 1);
      }
    },
    // A quick squeeze of the pointer, the click itself.
    press: function() {
      var s = document.getElementById('agentctl_showcase_cursor_svg');
      try {
        if (s) s.animate([{ transform: 'scale(1)' }, { transform: 'scale(0.72)' }, { transform: 'scale(1)' }], { duration: 260 });
      } catch (e) {}
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
      h.style.setProperty('left', (x + 22) + 'px');
      h.style.setProperty('top', (y + 12) + 'px');
      h.style.setProperty('opacity', '1');
      hudTimer = setTimeout(function() {
        h.style.setProperty('opacity', '0');
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
    // glideMs 0: the cursor snaps (the caller has already moved it, with real
    // mouse events). rippleMs 0: no ripple. beatMs: how long the click waits
    // after the ripple starts so it is on screen when the page reacts.
    act: async function(x, y, action, value, glideMs, rippleMs, beatMs, withHud, el, secret) {
      if (withHud) {
        var label = action;
        var icon = '👆';
        if (action === 'type' || value) {
          var shown = this.sensitive(el, secret)
            ? '••••'
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
        this.place(x, y);
      }
      if (rippleMs > 0 && (action === 'click' || action === 'double_click')) {
        this.ripple(x, y, rippleMs);
        this.press();
        if (beatMs > 0) await sleep(beatMs);
      }
    }
  };
  window.__agentctl_showcase_ready = true;
})();
"####;

/// Page-side check that the overlay exists, true in any world (the DOM is
/// shared; the API object lives in the world that installed it).
pub const JS_SHOWCASE_RENDERED: &str =
    "!!(window.__agentctl_showcase && document.getElementById('agentctl_showcase_cursor'))";

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

    /// The overlay is built from DOM nodes. These assert the property on the
    /// shipped script; the live Chrome tests prove it executes (also under
    /// Trusted Types).
    #[test]
    fn overlay_never_parses_html_or_needs_a_style_element() {
        assert!(
            !JS_SHOWCASE_ENGINE.contains("innerHTML"),
            "innerHTML throws under Trusted Types"
        );
        assert!(!JS_SHOWCASE_ENGINE.contains("createElement('style')"));
        assert!(JS_SHOWCASE_ENGINE.contains("t.textContent = String(text"));
    }

    #[test]
    fn ready_flag_is_set_only_after_the_overlay_is_built() {
        let built = JS_SHOWCASE_ENGINE.find("document.body.appendChild(root)");
        let flag = JS_SHOWCASE_ENGINE.find("window.__agentctl_showcase_ready = true");
        assert!(built.is_some() && flag.is_some() && built < flag);
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
        assert_eq!(cine.ripple_ms(), 900);

        let demo = ShowcaseConfig::demo();
        assert!(demo.enabled);
        assert_eq!(demo.glide_ms(), 220);
        assert_eq!(demo.ripple_ms(), 700);

        let snappy = ShowcaseConfig::snappy();
        assert!(snappy.enabled);
        assert_eq!(snappy.glide_ms(), 120);
        assert_eq!(snappy.ripple_ms(), 300);

        let off = ShowcaseConfig::default();
        assert!(!off.enabled);
        assert_eq!(off.glide_ms(), 0);
        assert_eq!(off.ripple_ms(), 0);
        assert_eq!(off.cursor_size, 32);
    }

    #[test]
    fn ripples_are_long_enough_to_film() {
        // Cinematic and demo must outlast a few frames of a 30 fps recording.
        assert!(ShowcaseSpeed::Cinematic.ripple_ms() >= 600);
        assert!(ShowcaseSpeed::Demo.ripple_ms() >= 600);
        assert_eq!(ShowcaseSpeed::Off.ripple_ms(), 0);
        let mut c = ShowcaseConfig::demo();
        c.click_ripple = false;
        assert_eq!(c.ripple_ms(), 0);
    }

    #[test]
    fn ripple_beat_is_a_quarter_capped() {
        assert_eq!(ripple_beat_ms(0), 0);
        assert_eq!(ripple_beat_ms(300), 75);
        assert_eq!(ripple_beat_ms(700), 175);
        assert_eq!(ripple_beat_ms(900), 200);
        assert_eq!(ripple_beat_ms(5000), 200);
    }

    #[test]
    fn glide_and_size_are_clamped() {
        assert_eq!(clamp_glide_ms(0), 0);
        assert_eq!(clamp_glide_ms(500), 500);
        assert_eq!(clamp_glide_ms(u64::MAX), MAX_GLIDE_MS);
        let mut c = ShowcaseConfig::demo();
        c.custom_glide_ms = Some(999_999);
        assert_eq!(c.glide_ms(), 3000);
        assert_eq!(clamp_cursor_size(1), 16);
        assert_eq!(clamp_cursor_size(32), 32);
        assert_eq!(clamp_cursor_size(10_000), 96);
        c.cursor_size = 500;
        assert_eq!(c.to_json()["cursor_size"], 96);
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

    #[test]
    fn every_style_looks_different() {
        let all = [
            CursorStyle::GlowArrow,
            CursorStyle::NeonCyan,
            CursorStyle::MinimalDot,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a.visual(), b.visual(), "{a:?} and {b:?} look the same");
                assert_ne!(a.visual().accent, b.visual().accent);
                assert_ne!(a.visual().glow, b.visual().glow);
            }
        }
        assert_eq!(CursorStyle::MinimalDot.visual().shape, CursorShape::Dot);
        assert_eq!(CursorStyle::NeonCyan.visual().shape, CursorShape::Arrow);
        assert!(CursorStyle::NeonCyan.visual().glow.contains("#22d3ee"));
    }

    #[test]
    fn style_and_size_reach_the_script() {
        let mut c = ShowcaseConfig::demo();
        c.cursor_style = CursorStyle::NeonCyan;
        c.cursor_size = 48;
        let js = c.engine_js();
        assert!(!js.contains("__AGENTCTL_SHOWCASE_CONFIG__"));
        assert!(js.contains(r#""style":"neon_cyan""#));
        assert!(js.contains(r#""size":48"#));
        assert!(js.contains("#22d3ee"));
        c.cursor_style = CursorStyle::MinimalDot;
        assert!(c.engine_js().contains(r#""dot":true"#));
    }

    #[test]
    fn visual_args_are_parsed_and_clamped() {
        let mut c = ShowcaseConfig::demo();
        c.apply_visual_args(
            &json!({ "cursor_style": "neon", "glide_ms": 99999, "cursor_size": 4 }),
        );
        assert_eq!(c.cursor_style, CursorStyle::NeonCyan);
        assert_eq!(c.custom_glide_ms, Some(3000));
        assert_eq!(c.cursor_size, 16);
        c.apply_visual_args(&json!({ "cursor_style": "nonsense", "cursor_size": 64 }));
        assert_eq!(
            c.cursor_style,
            CursorStyle::NeonCyan,
            "unknown style is ignored"
        );
        assert_eq!(c.cursor_size, 64);
        assert_eq!(
            c.custom_glide_ms,
            Some(3000),
            "absent args leave values alone"
        );
        c.apply_visual_args(&json!({ "cursor_size": 1000 }));
        assert_eq!(c.cursor_size, 96);
    }

    #[test]
    fn for_speed_enables_except_off() {
        let c = ShowcaseConfig::for_speed(ShowcaseSpeed::Cinematic);
        assert!(c.enabled);
        assert_eq!(c.speed, ShowcaseSpeed::Cinematic);
        assert!(!ShowcaseConfig::for_speed(ShowcaseSpeed::Off).enabled);
    }

    #[test]
    fn glide_path_is_eased_monotonic_and_ends_on_target() {
        let from = (10.0, 20.0);
        let to = (410.0, 220.0);
        let p = glide_path(from, to, 350);
        assert_eq!(p.len(), glide_step_count(350));
        assert_eq!(*p.last().unwrap(), to);
        let mut prev = from.0;
        for (x, y) in &p {
            assert!(*x >= prev, "x went backwards");
            assert!((*y - from.1) / (to.1 - from.1) <= 1.0 + 1e-9);
            prev = *x;
        }
        // Ease-out: the first step covers more than a linear step would.
        let linear = (to.0 - from.0) / p.len() as f64;
        assert!(p[0].0 - from.0 > linear);
        // Moving left/up works too.
        let back = glide_path(to, from, 200);
        assert_eq!(*back.last().unwrap(), from);
        assert!(back[0].0 < to.0);
    }

    #[test]
    fn glide_path_step_count_is_bounded() {
        assert_eq!(glide_path((0.0, 0.0), (5.0, 5.0), 0), vec![(5.0, 5.0)]);
        assert_eq!(glide_step_count(1), 1);
        assert_eq!(glide_step_count(160), 10);
        assert_eq!(glide_step_count(3000), 40);
        assert_eq!(glide_step_count(u64::MAX), 40);
        assert!((glide_ease(0.0)).abs() < 1e-9);
        assert!((glide_ease(1.0) - 1.0).abs() < 1e-9);
    }
}
