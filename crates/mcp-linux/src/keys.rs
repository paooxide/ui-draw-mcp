//! Key names and combos, translated into X keysyms for the RemoteDesktop
//! portal.
//!
//! The portal takes keysyms rather than keycodes, and the compositor maps a
//! keysym onto whatever keycode and shift level the active layout needs
//! (Mutter reserves spare keycodes for symbols the layout lacks), so typing
//! is layout-independent and needs no keymap here.
//!
//! The vocabulary is the one `keyboard_shortcut` documents, shared with the
//! macOS backend so an agent does not learn two spellings. `cmd` is
//! translated to Control: on Linux the copy key is Ctrl+C, and an agent that
//! asks for `cmd+c` means copy, not the Super key. `super` and `meta` name
//! the Super key explicitly.

use xkeysym::{key, Keysym};

/// A modifier as the portal will press it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modifier {
    Control,
    Shift,
    Alt,
    Super,
}

impl Modifier {
    pub fn keysym(self) -> Keysym {
        match self {
            Modifier::Control => Keysym::Control_L,
            Modifier::Shift => Keysym::Shift_L,
            Modifier::Alt => Keysym::Alt_L,
            Modifier::Super => Keysym::Super_L,
        }
    }

    /// Parse a modifier name from the shared vocabulary.
    pub fn parse(name: &str) -> Option<Modifier> {
        Some(match name {
            "cmd" | "command" | "ctrl" | "control" => Modifier::Control,
            "shift" => Modifier::Shift,
            "opt" | "option" | "alt" => Modifier::Alt,
            "super" | "meta" | "win" | "windows" => Modifier::Super,
            // `fn` has no keysym; it is a hardware modifier on laptops and
            // the compositor never sees it.
            _ => return None,
        })
    }
}

/// A parsed combo: modifiers to hold, then one key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Combo {
    pub modifiers: Vec<Modifier>,
    pub key: Keysym,
}

/// The keysym for a named key, or a single character.
pub fn keysym_for(name: &str) -> Option<Keysym> {
    let sym = match name {
        "return" | "enter" => key::Return,
        "tab" => key::Tab,
        "space" => key::space,
        "delete" | "backspace" => key::BackSpace,
        "forwarddelete" | "del" => key::Delete,
        "escape" | "esc" => key::Escape,
        "left" => key::Left,
        "right" => key::Right,
        "up" => key::Up,
        "down" => key::Down,
        "home" => key::Home,
        "end" => key::End,
        "pageup" | "page_up" => key::Page_Up,
        "pagedown" | "page_down" => key::Page_Down,
        "insert" => key::Insert,
        "printscreen" | "print" => key::Print,
        "capslock" => key::Caps_Lock,
        "f1" => key::F1,
        "f2" => key::F2,
        "f3" => key::F3,
        "f4" => key::F4,
        "f5" => key::F5,
        "f6" => key::F6,
        "f7" => key::F7,
        "f8" => key::F8,
        "f9" => key::F9,
        "f10" => key::F10,
        "f11" => key::F11,
        "f12" => key::F12,
        "minus" => key::minus,
        "equal" | "equals" => key::equal,
        "comma" => key::comma,
        "period" => key::period,
        "slash" => key::slash,
        "backslash" => key::backslash,
        "semicolon" => key::semicolon,
        "quote" => key::apostrophe,
        "grave" | "backtick" => key::grave,
        "leftbracket" => key::bracketleft,
        "rightbracket" => key::bracketright,
        _ => {
            let mut chars = name.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            return Some(Keysym::from_char(c));
        }
    };
    Some(Keysym::new(sym))
}

/// Parse `ctrl+shift+n`. Every segment but the last must be a modifier; the
/// last is the key. A combo of only modifiers is rejected: there is nothing
/// to press.
pub fn parse_combo(combo: &str) -> Result<Combo, String> {
    let segments: Vec<&str> = combo.split('+').collect();
    let Some((last, mods)) = segments.split_last() else {
        return Err("empty combo".into());
    };
    let mut modifiers = Vec::new();
    for m in mods {
        match Modifier::parse(m) {
            Some(modifier) => {
                if !modifiers.contains(&modifier) {
                    modifiers.push(modifier);
                }
            }
            None if *m == "fn" => {}
            None => {
                return Err(format!(
                    "'{m}' is not a modifier (cmd, ctrl, shift, alt, super)"
                ))
            }
        }
    }
    if Modifier::parse(last).is_some() {
        return Err(format!(
            "'{combo}' names only modifiers; the last segment must be a key"
        ));
    }
    let key = keysym_for(last).ok_or_else(|| format!("unknown key '{last}'"))?;
    Ok(Combo { modifiers, key })
}

/// Keysyms to type a string, one per character. Newlines become Return and
/// tabs become Tab, which is what a person typing them would press.
pub fn keysyms_for_text(text: &str) -> Vec<Keysym> {
    text.chars()
        .map(|c| match c {
            '\n' => Keysym::Return,
            '\r' => Keysym::Return,
            '\t' => Keysym::Tab,
            c => Keysym::from_char(c),
        })
        .collect()
}

/// Modifiers named on a pointer action (`cmd+click`), same vocabulary.
pub fn pointer_modifiers(names: &[String]) -> Result<Vec<Modifier>, String> {
    let mut out = Vec::new();
    for n in names {
        match Modifier::parse(n) {
            Some(m) => {
                if !out.contains(&m) {
                    out.push(m)
                }
            }
            None if n == "fn" => {}
            None => return Err(format!("unknown modifier '{n}'")),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combos_parse_into_modifiers_and_a_key() {
        let c = parse_combo("ctrl+shift+n").unwrap();
        assert_eq!(c.modifiers, vec![Modifier::Control, Modifier::Shift]);
        assert_eq!(c.key, Keysym::n);
        let c = parse_combo("return").unwrap();
        assert!(c.modifiers.is_empty());
        assert_eq!(c.key, Keysym::Return);
        let c = parse_combo("alt+f4").unwrap();
        assert_eq!(c.modifiers, vec![Modifier::Alt]);
        assert_eq!(c.key, Keysym::F4);
    }

    /// The whole point of the translation: an agent that learned `cmd+c`
    /// must copy on Linux too.
    #[test]
    fn cmd_means_control_and_super_means_super() {
        assert_eq!(
            parse_combo("cmd+c").unwrap().modifiers,
            vec![Modifier::Control]
        );
        assert_eq!(
            parse_combo("command+v").unwrap().modifiers,
            vec![Modifier::Control]
        );
        assert_eq!(
            parse_combo("super+h").unwrap().modifiers,
            vec![Modifier::Super]
        );
        assert_eq!(
            parse_combo("meta+up").unwrap().modifiers,
            vec![Modifier::Super]
        );
        // cmd and ctrl together collapse to one Control press.
        assert_eq!(
            parse_combo("cmd+ctrl+a").unwrap().modifiers,
            vec![Modifier::Control]
        );
    }

    #[test]
    fn combos_of_only_modifiers_and_unknown_keys_are_rejected() {
        assert!(parse_combo("cmd+shift").is_err());
        assert!(parse_combo("shift").is_err());
        assert!(parse_combo("").is_err());
        assert!(parse_combo("cmd+").is_err());
        assert!(parse_combo("cmd+nosuchkey").is_err());
        assert!(parse_combo("hyper+a").is_err());
        // `fn` is accepted and ignored, matching the macOS backend.
        assert_eq!(parse_combo("fn+f1").unwrap().modifiers, vec![]);
    }

    #[test]
    fn named_keys_and_single_characters_resolve() {
        assert_eq!(keysym_for("pageup"), Some(Keysym::Page_Up));
        assert_eq!(keysym_for("page_up"), Some(Keysym::Page_Up));
        assert_eq!(keysym_for("esc"), Some(Keysym::Escape));
        assert_eq!(keysym_for("a"), Some(Keysym::a));
        assert_eq!(keysym_for("1"), Some(Keysym::_1));
        assert_eq!(keysym_for("é"), Some(Keysym::from_char('é')));
        assert_eq!(keysym_for("ab"), None);
        assert_eq!(keysym_for(""), None);
    }

    #[test]
    fn typed_text_maps_every_char_with_newline_as_return() {
        let syms = keysyms_for_text("a\nB\t€");
        assert_eq!(syms.len(), 5);
        assert_eq!(syms[0], Keysym::a);
        assert_eq!(syms[1], Keysym::Return);
        assert_eq!(syms[2], Keysym::B);
        assert_eq!(syms[3], Keysym::Tab);
        assert_eq!(syms[4], Keysym::from_char('€'));
        assert!(keysyms_for_text("").is_empty());
    }

    #[test]
    fn pointer_modifiers_share_the_vocabulary() {
        let m = pointer_modifiers(&["cmd".into(), "shift".into(), "shift".into()]).unwrap();
        assert_eq!(m, vec![Modifier::Control, Modifier::Shift]);
        assert!(pointer_modifiers(&["hyper".into()]).is_err());
        assert!(pointer_modifiers(&[]).unwrap().is_empty());
    }
}
