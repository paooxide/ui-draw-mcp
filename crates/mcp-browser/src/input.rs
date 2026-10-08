//! Pure parts of `browser_act`'s real input: action names, key combos and the
//! geometry of a drag or a scroll. Nothing here touches a browser, so all of
//! it is tested as free functions; the CDP calls that use it live in
//! `backend.rs`.

use crate::backend::key_event_spec;

/// Every action `browser_act` takes, as it is spelled in the schema.
pub(crate) const ACTIONS: [&str; 13] = [
    "click",
    "double_click",
    "triple_click",
    "right_click",
    "hover",
    "drag",
    "scroll",
    "type",
    "select",
    "focus",
    "scroll_into_view",
    "submit",
    "press",
];

/// The canonical action for what a model wrote. Case, `-`, `_` and spaces do
/// not matter, and the spellings models reach for (`key`, `dblclick`,
/// `drag_and_drop`) are accepted. The error lists the valid actions.
pub(crate) fn canonical_action(raw: &str) -> Result<&'static str, String> {
    let norm: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c == '-' || c == ' ' {
                '_'
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect();
    let squashed = norm.replace('_', "");
    if let Some(a) = ACTIONS.iter().find(|a| **a == norm) {
        return Ok(a);
    }
    let alias = match squashed.as_str() {
        "key" | "keypress" | "hotkey" | "shortcut" | "presskey" => "press",
        "doubleclick" | "dblclick" => "double_click",
        "tripleclick" | "tripleclk" => "triple_click",
        "rightclick" | "contextclick" | "contextmenu" => "right_click",
        "draganddrop" | "dragdrop" | "dragto" => "drag",
        "scrollto" | "scrollintoview" => "scroll_into_view",
        _ => {
            return Err(format!(
                "unknown action '{raw}'; use one of {}",
                ACTIONS.join(", ")
            ))
        }
    };
    Ok(alias)
}

/// CDP `modifiers` bits.
const ALT: u32 = 1;
const CTRL: u32 = 2;
const META: u32 = 4;
const SHIFT: u32 = 8;

/// A modifier key, for the `keyDown`/`keyUp` pair sent around the real key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Modifier {
    pub bit: u32,
    pub key: &'static str,
    pub code: &'static str,
    pub vk: u32,
}

const MOD_ALT: Modifier = Modifier {
    bit: ALT,
    key: "Alt",
    code: "AltLeft",
    vk: 18,
};
const MOD_CTRL: Modifier = Modifier {
    bit: CTRL,
    key: "Control",
    code: "ControlLeft",
    vk: 17,
};
const MOD_META: Modifier = Modifier {
    bit: META,
    key: "Meta",
    code: "MetaLeft",
    vk: 91,
};
const MOD_SHIFT: Modifier = Modifier {
    bit: SHIFT,
    key: "Shift",
    code: "ShiftLeft",
    vk: 16,
};

fn modifier(name: &str) -> Option<Modifier> {
    match name.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => Some(MOD_CTRL),
        "alt" | "option" | "opt" => Some(MOD_ALT),
        "shift" => Some(MOD_SHIFT),
        "meta" | "cmd" | "command" | "super" | "win" | "windows" => Some(MOD_META),
        _ => None,
    }
}

/// CDP key-event parameters for one key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Key {
    pub key: String,
    pub code: String,
    pub vk: u32,
    pub text: Option<String>,
}

/// A key with the modifiers held while it is pressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeyCombo {
    pub mods: Vec<Modifier>,
    pub key: Key,
}

impl KeyCombo {
    /// The CDP `modifiers` bitmask.
    pub fn bits(&self) -> u32 {
        self.mods.iter().fold(0, |b, m| b | m.bit)
    }

    /// The editing command a browser that ignores the key event's own
    /// default action (headless Chrome on macOS) must be told to run.
    pub fn edit_command(&self) -> Option<&'static str> {
        let b = self.bits();
        if b & (CTRL | META) == 0 || b & ALT != 0 {
            return None;
        }
        match (self.key.key.to_ascii_lowercase().as_str(), b & SHIFT != 0) {
            ("a", false) => Some("selectAll"),
            ("c", false) => Some("copy"),
            ("x", false) => Some("cut"),
            ("v", false) => Some("paste"),
            ("z", false) => Some("undo"),
            ("z", true) | ("y", false) => Some("redo"),
            _ => None,
        }
    }
}

const KEY_HELP: &str = "a single character, a named key (Enter, Escape, Tab, Backspace, Delete, Insert, Space, ArrowUp/Down/Left/Right, Home, End, PageUp, PageDown, F1-F12; aliases Esc, Return, Del, Up, Down, Left, Right, PgUp, PgDn), or a combo with modifiers ctrl, alt, shift, meta joined by '+', e.g. ctrl+a, Shift+ArrowDown, cmd+shift+z";

/// Parse `value` of a `press`: a key, or modifiers and a key joined by `+`
/// (`-` too, when every part before the last is a modifier).
pub(crate) fn parse_key_combo(raw: &str) -> Result<KeyCombo, String> {
    // A lone space is the Space key; anything else is trimmed.
    let s = if !raw.is_empty() && raw.chars().all(|c| c == ' ') {
        " "
    } else {
        raw.trim()
    };
    if s.is_empty() {
        return Err(format!("press needs a key in 'value': {KEY_HELP}"));
    }
    for sep in ['+', '-'] {
        if s.chars().count() < 2 || !s.contains(sep) {
            continue;
        }
        if let Some(parts) = split_combo(s, sep) {
            let (last, rest) = parts.split_last().expect("split_combo returns parts");
            let mods: Option<Vec<Modifier>> = rest.iter().map(|p| modifier(p)).collect();
            if let Some(mods) = mods {
                return finish(mods, last, raw);
            }
            if sep == '+' {
                let bad = rest.iter().find(|p| modifier(p).is_none()).unwrap();
                return Err(format!(
                    "'{bad}' in '{raw}' is not a modifier (ctrl, alt, shift, meta); press takes {KEY_HELP}"
                ));
            }
        }
    }
    finish(Vec::new(), s, raw)
}

/// `a+b+c` into parts; a trailing separator is the key itself (`ctrl++`).
fn split_combo(s: &str, sep: char) -> Option<Vec<String>> {
    let mut body = s;
    let mut last = None;
    if body.ends_with(sep) {
        last = Some(sep.to_string());
        body = &body[..body.len() - 1];
        if body.ends_with(sep) {
            body = &body[..body.len() - 1];
        }
    }
    let mut parts: Vec<String> = if body.is_empty() {
        Vec::new()
    } else {
        body.split(sep).map(str::to_string).collect()
    };
    if parts.iter().any(|p| p.is_empty()) {
        return None;
    }
    parts.extend(last);
    (!parts.is_empty()).then_some(parts)
}

fn finish(mods: Vec<Modifier>, key: &str, raw: &str) -> Result<KeyCombo, String> {
    let bits = mods.iter().fold(0, |b, m| b | m.bit);
    let key = resolve_key(key, bits).ok_or_else(|| {
        format!(
            "unknown key '{}' in '{raw}'; press takes {KEY_HELP}",
            key.trim()
        )
    })?;
    Ok(KeyCombo { mods, key })
}

fn from_spec(name: &str, with_text: bool) -> Option<Key> {
    let k = key_event_spec(name)?;
    Some(Key {
        key: k.key.into(),
        code: k.code.into(),
        vk: k.vk,
        text: k.text.filter(|_| with_text).map(Into::into),
    })
}

/// One key by name or character. `bits` are the modifiers held: they decide
/// whether the key types text, and the case of a letter.
fn resolve_key(name: &str, bits: u32) -> Option<Key> {
    let typing = bits & (CTRL | ALT | META) == 0;
    let canonical = match name.to_ascii_lowercase().as_str() {
        "enter" | "return" => Some("Enter"),
        "escape" | "esc" => Some("Escape"),
        "tab" => Some("Tab"),
        "arrowdown" | "down" => Some("ArrowDown"),
        "arrowup" | "up" => Some("ArrowUp"),
        "arrowleft" | "left" => Some("ArrowLeft"),
        "arrowright" | "right" => Some("ArrowRight"),
        "home" => Some("Home"),
        "end" => Some("End"),
        "pageup" | "pgup" => Some("PageUp"),
        "pagedown" | "pgdn" | "pgdown" => Some("PageDown"),
        "backspace" => Some("Backspace"),
        "delete" | "del" => Some("Delete"),
        "insert" | "ins" => Some("Insert"),
        "space" | "spacebar" => Some("Space"),
        _ => None,
    };
    if let Some(c) = canonical {
        return from_spec(c, typing);
    }
    let lower = name.to_ascii_lowercase();
    if let Some(n) = lower
        .strip_prefix('f')
        .and_then(|n| n.parse::<u32>().ok())
        .filter(|n| (1..=12).contains(n) && name.len() >= 2)
    {
        let label = format!("F{n}");
        return Some(Key {
            key: label.clone(),
            code: label,
            vk: 111 + n,
            text: None,
        });
    }
    let mut chars = name.chars();
    let ch = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    if ch == ' ' {
        return from_spec("Space", typing);
    }
    if ch.is_control() {
        return None;
    }
    // A letter's case follows the modifiers, not the spelling: Control+A is
    // select-all, which the page sees as key "a"; Shift makes it "A".
    let ch = if ch.is_ascii_alphabetic() {
        if bits & SHIFT != 0 {
            ch.to_ascii_uppercase()
        } else if bits & (CTRL | ALT | META) != 0 {
            ch.to_ascii_lowercase()
        } else {
            ch
        }
    } else {
        ch
    };
    let (code, vk) = if ch.is_ascii_alphabetic() {
        (
            format!("Key{}", ch.to_ascii_uppercase()),
            ch.to_ascii_uppercase() as u32,
        )
    } else if ch.is_ascii_digit() {
        (format!("Digit{ch}"), ch as u32)
    } else {
        match ch {
            ';' => ("Semicolon".to_string(), 186),
            '=' => ("Equal".into(), 187),
            ',' => ("Comma".into(), 188),
            '-' => ("Minus".into(), 189),
            '.' => ("Period".into(), 190),
            '/' => ("Slash".into(), 191),
            '`' => ("Backquote".into(), 192),
            '[' => ("BracketLeft".into(), 219),
            '\\' => ("Backslash".into(), 220),
            ']' => ("BracketRight".into(), 221),
            '\'' => ("Quote".into(), 222),
            _ => (String::new(), 0),
        }
    };
    Some(Key {
        key: ch.to_string(),
        code,
        vk,
        text: typing.then(|| ch.to_string()),
    })
}

/// A coordinate argument: a number, or a numeric string (models quote them).
pub(crate) fn coord(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
    .filter(|n| n.is_finite())
}

/// An element's box in viewport CSS px.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Rect {
    pub left: f64,
    pub top: f64,
    pub width: f64,
    pub height: f64,
}

/// The point a pointer action means. With a box, `x`/`y` are offsets from its
/// top-left corner and a missing one is the middle; with none they are
/// viewport points and both are needed.
pub(crate) fn point_in(
    rect: Option<Rect>,
    x: Option<f64>,
    y: Option<f64>,
) -> Result<(f64, f64), String> {
    match (rect, x, y) {
        (Some(r), x, y) => Ok((
            r.left + x.unwrap_or(r.width / 2.0),
            r.top + y.unwrap_or(r.height / 2.0),
        )),
        (None, Some(x), Some(y)) => Ok((x, y)),
        (None, _, _) => {
            Err("without ref or query, x and y (viewport CSS px) are both needed".into())
        }
    }
}

/// Real input only lands inside the viewport; say so rather than click
/// nothing.
pub(crate) fn check_in_viewport(at: (f64, f64), vw: f64, vh: f64) -> Result<(), String> {
    if at.0 >= 0.0 && at.1 >= 0.0 && at.0 < vw && at.1 < vh {
        return Ok(());
    }
    Err(format!(
        "point ({:.1}, {:.1}) is outside the {vw}x{vh} viewport; nothing was done (with a ref or query, x and y are offsets from the element's top-left corner)",
        at.0, at.1
    ))
}

/// The middle of the part of `r` that is on screen, `None` when none is.
pub(crate) fn visible_center(r: Rect, vw: f64, vh: f64) -> Option<(f64, f64)> {
    let (l, t) = (r.left.max(0.0), r.top.max(0.0));
    let (rt, b) = ((r.left + r.width).min(vw), (r.top + r.height).min(vh));
    (rt > l && b > t).then(|| ((l + rt) / 2.0, (t + b) / 2.0))
}

/// The points of a drag from `from` to `to`: `steps` evenly spaced moves, the
/// last of them exactly on `to`.
pub(crate) fn drag_path(from: (f64, f64), to: (f64, f64), steps: usize) -> Vec<(f64, f64)> {
    let n = steps.max(1);
    (1..=n)
        .map(|i| {
            if i == n {
                return to;
            }
            let t = i as f64 / n as f64;
            (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t)
        })
        .collect()
}

/// How far to wheel-scroll: `(delta_x, delta_y)` in CSS px. Explicit `dx` /
/// `dy` win; else `value` names a direction ("up", "down", "left", "right",
/// "top", "bottom", "pagedown"...); else one viewport height down. `top` and
/// `bottom` are far past any page, since the wheel stops at the end.
pub(crate) fn scroll_delta(
    value: Option<&str>,
    dx: Option<f64>,
    dy: Option<f64>,
    viewport: (f64, f64),
) -> Result<(f64, f64), String> {
    if dx.is_some() || dy.is_some() {
        return Ok((dx.unwrap_or(0.0), dy.unwrap_or(0.0)));
    }
    const FAR: f64 = 1_000_000.0;
    match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        None | Some("") | Some("down") | Some("pagedown") => Ok((0.0, viewport.1)),
        Some("up") | Some("pageup") => Ok((0.0, -viewport.1)),
        Some("left") => Ok((-viewport.0, 0.0)),
        Some("right") => Ok((viewport.0, 0.0)),
        Some("top") | Some("home") => Ok((0.0, -FAR)),
        Some("bottom") | Some("end") => Ok((0.0, FAR)),
        Some(other) => Err(format!(
            "unknown scroll direction '{other}'; use up, down, left, right, top or bottom, or pass dx and dy in CSS px"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_are_forgiving_about_spelling() {
        for (raw, want) in [
            ("click", "click"),
            ("Click", "click"),
            ("key", "press"),
            ("keypress", "press"),
            ("key_press", "press"),
            ("hotkey", "press"),
            ("shortcut", "press"),
            ("double-click", "double_click"),
            ("dblclick", "double_click"),
            ("DoubleClick", "double_click"),
            ("triple-click", "triple_click"),
            ("triple_click", "triple_click"),
            ("tripleclick", "triple_click"),
            ("right-click", "right_click"),
            ("rightclick", "right_click"),
            ("context_click", "right_click"),
            ("contextmenu", "right_click"),
            ("drag_and_drop", "drag"),
            ("drag-and-drop", "drag"),
            ("dragdrop", "drag"),
            ("drag_to", "drag"),
            ("scroll_to", "scroll_into_view"),
            ("scrollIntoView", "scroll_into_view"),
            ("scroll-into-view", "scroll_into_view"),
            ("scroll", "scroll"),
        ] {
            assert_eq!(canonical_action(raw), Ok(want), "{raw}");
        }
        let e = canonical_action("teleport").unwrap_err();
        assert!(e.contains("'teleport'"), "{e}");
        for a in ACTIONS {
            assert!(e.contains(a), "{a} missing from {e}");
        }
    }

    fn combo(s: &str) -> KeyCombo {
        parse_key_combo(s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn combos_parse_with_modifiers_and_aliases() {
        let c = combo("ctrl+a");
        assert_eq!((c.bits(), c.key.key.as_str(), c.key.vk), (2, "a", 65));
        assert_eq!(c.key.code, "KeyA");
        assert_eq!(c.key.text, None, "a shortcut types nothing");
        // Spelling and case do not matter; Control+A is select-all, key "a".
        for s in ["Control+A", "CTRL+a", "control-a", "ctrl-A"] {
            assert_eq!(combo(s), c, "{s}");
        }
        assert_eq!(combo("cmd+shift+z").bits(), 4 | 8);
        assert_eq!(combo("Meta+Enter").key.key, "Enter");
        assert_eq!(combo("Meta+Enter").key.text, None);
        assert_eq!(combo("Shift+ArrowDown").key.vk, 40);
        assert_eq!(combo("alt+Tab").bits(), 1);
        assert_eq!(combo("ctrl+End").key.key, "End");
        assert_eq!(combo("option+opt+super+x").bits(), 1 | 4);
        // Shift makes a letter upper case and it still types.
        let s = combo("shift+a");
        assert_eq!(
            (s.key.key.as_str(), s.key.text.as_deref()),
            ("A", Some("A"))
        );
        // The separator can be the key.
        assert_eq!(combo("ctrl++").key.key, "+");
        assert_eq!(combo("+").key.key, "+");
        assert_eq!(combo("-").key.key, "-");
        assert_eq!(combo("ctrl+-").key.key, "-");
        assert_eq!(combo("ctrl--").key.key, "-");
    }

    #[test]
    fn single_keys_cover_names_characters_and_function_keys() {
        for (s, key, vk) in [
            ("Esc", "Escape", 27),
            ("return", "Enter", 13),
            ("Del", "Delete", 46),
            ("up", "ArrowUp", 38),
            ("DOWN", "ArrowDown", 40),
            ("Left", "ArrowLeft", 37),
            ("right", "ArrowRight", 39),
            ("PgUp", "PageUp", 33),
            ("pgdn", "PageDown", 34),
            ("Insert", "Insert", 45),
            ("F1", "F1", 112),
            ("f12", "F12", 123),
            ("Spacebar", " ", 32),
            (" ", " ", 32),
            ("1", "1", 49),
            ("/", "/", 191),
            ("a", "a", 65),
            ("Z", "Z", 90),
        ] {
            let k = combo(s).key;
            assert_eq!((k.key.as_str(), k.vk), (key, vk), "{s}");
        }
        assert_eq!(combo("a").key.text.as_deref(), Some("a"));
        assert_eq!(combo("Enter").key.text.as_deref(), Some("\r"));
        assert_eq!(combo("Space").key.text.as_deref(), Some(" "));
        assert_eq!(combo("End").key.text, None);
    }

    #[test]
    fn bad_keys_say_what_is_accepted() {
        for bad in ["", "ctrl+nope", "F13", "foo+a", "ctrl+a+", "Arrow-Down"] {
            let e = parse_key_combo(bad).unwrap_err();
            assert!(e.contains("ctrl+a") && e.contains("Escape"), "{bad}: {e}");
        }
        assert!(parse_key_combo("foo+a").unwrap_err().contains("'foo'"));
    }

    #[test]
    fn editing_shortcuts_name_their_command() {
        for (s, cmd) in [
            ("ctrl+a", Some("selectAll")),
            ("cmd+a", Some("selectAll")),
            ("Control+C", Some("copy")),
            ("meta+x", Some("cut")),
            ("ctrl+v", Some("paste")),
            ("ctrl+z", Some("undo")),
            ("ctrl+shift+z", Some("redo")),
            ("cmd+y", Some("redo")),
            ("a", None),
            ("ctrl+b", None),
            ("alt+ctrl+a", None),
            ("shift+a", None),
        ] {
            assert_eq!(combo(s).edit_command(), cmd, "{s}");
        }
    }

    #[test]
    fn coordinates_may_be_quoted_numbers() {
        use serde_json::json;
        assert_eq!(coord(&json!(60)), Some(60.0));
        assert_eq!(coord(&json!("115")), Some(115.0));
        assert_eq!(coord(&json!(" 7.5 ")), Some(7.5));
        assert_eq!(coord(&json!("-3")), Some(-3.0));
        assert_eq!(coord(&json!("abc")), None);
        assert_eq!(coord(&json!("NaN")), None);
        assert_eq!(coord(&json!(null)), None);
        assert_eq!(coord(&json!(true)), None);
    }

    #[test]
    fn a_drag_path_is_even_and_ends_on_the_target() {
        let p = drag_path((0.0, 0.0), (100.0, 50.0), 10);
        assert_eq!(p.len(), 10);
        assert_eq!(p[0], (10.0, 5.0));
        assert_eq!(*p.last().unwrap(), (100.0, 50.0));
        assert_eq!(drag_path((1.0, 1.0), (1.0, 1.0), 0), vec![(1.0, 1.0)]);
    }

    #[test]
    fn scroll_defaults_to_a_page_down() {
        let vp = (800.0, 600.0);
        assert_eq!(scroll_delta(None, None, None, vp), Ok((0.0, 600.0)));
        assert_eq!(scroll_delta(Some("up"), None, None, vp), Ok((0.0, -600.0)));
        assert_eq!(
            scroll_delta(Some("Right"), None, None, vp),
            Ok((800.0, 0.0))
        );
        assert_eq!(scroll_delta(Some("top"), None, None, vp).unwrap().1, -1e6);
        assert_eq!(scroll_delta(Some("bottom"), None, None, vp).unwrap().1, 1e6);
        // dx/dy beat a direction, and a missing one is zero.
        assert_eq!(
            scroll_delta(Some("up"), Some(30.0), None, vp),
            Ok((30.0, 0.0))
        );
        assert!(scroll_delta(Some("sideways"), None, None, vp).is_err());
    }

    #[test]
    fn points_are_offsets_in_a_box_and_viewport_px_without_one() {
        let r = Rect {
            left: 100.0,
            top: 50.0,
            width: 200.0,
            height: 80.0,
        };
        assert_eq!(
            point_in(Some(r), Some(60.0), Some(115.0)),
            Ok((160.0, 165.0))
        );
        assert_eq!(point_in(Some(r), None, None), Ok((200.0, 90.0)));
        assert_eq!(point_in(Some(r), Some(5.0), None), Ok((105.0, 90.0)));
        assert_eq!(point_in(None, Some(7.0), Some(9.0)), Ok((7.0, 9.0)));
        assert!(point_in(None, Some(7.0), None).is_err());
        assert!(point_in(None, None, None).is_err());
        assert!(check_in_viewport((10.0, 10.0), 800.0, 600.0).is_ok());
        assert!(check_in_viewport((800.0, 10.0), 800.0, 600.0).is_err());
        assert!(check_in_viewport((-1.0, 10.0), 800.0, 600.0).is_err());
        // A box taller than the screen is scrolled by its visible middle.
        let tall = Rect {
            left: 0.0,
            top: -100.0,
            width: 800.0,
            height: 5000.0,
        };
        assert_eq!(visible_center(tall, 800.0, 600.0), Some((400.0, 300.0)));
        let below = Rect {
            left: 0.0,
            top: 700.0,
            width: 10.0,
            height: 10.0,
        };
        assert_eq!(visible_center(below, 800.0, 600.0), None);
    }
}
