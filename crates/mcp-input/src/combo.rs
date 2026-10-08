//! Parsing a key combo, once, for every platform.
//!
//! `keyboard_shortcut` used to accept only `^([a-z0-9]+\+)*[a-z0-9]+$`, so the
//! spellings a model reaches for first (`Control+A`, `Cmd+Shift+Z`, `Return`,
//! `ctrl-a`, `PageUp`, `cmd++`) were refused outright, while a combo with two
//! keys (`a+b`) got through and pressed only one of them.
//!
//! This turns any of those spellings into a canonical string (`ctrl+a`,
//! `cmd+shift+z`, `pageup`) that the platform backends parse with nothing
//! cleverer than a split on `+`: the canonical form never contains a literal
//! `+` (that key is `plus`) and every key has exactly one name. The platforms
//! differ in what `mod` means and in which modifier names exist, not in the key
//! vocabulary, so only modifiers take an [`Os`].

/// Which platform's modifier conventions a combo is read under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Mac,
    Linux,
}

impl Os {
    /// From `InputBackend::platform()`. Anything that is not Linux reads as
    /// macOS: that is the vocabulary the tool documents.
    pub fn from_platform(platform: &str) -> Os {
        if platform == "linux" {
            Os::Linux
        } else {
            Os::Mac
        }
    }
}

/// A combo in canonical form: modifiers to hold, then exactly one key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Combo {
    /// Canonical modifier names, in the order given, without duplicates.
    pub modifiers: Vec<&'static str>,
    /// Canonical key name.
    pub key: String,
}

impl Combo {
    /// `cmd+shift+z`: what the backends receive.
    pub fn canonical(&self) -> String {
        let mut s = String::new();
        for m in &self.modifiers {
            s.push_str(m);
            s.push('+');
        }
        s.push_str(&self.key);
        s
    }
}

/// What a caller may use when a key name is not recognised.
pub const KEY_NAMES: &str = "a-z, 0-9, return, tab, space, escape, delete (backspace), \
    forwarddelete, insert, home, end, pageup, pagedown, up, down, left, right, f1-f20, \
    and punctuation as the character or its name (minus, equal, plus, comma, period, slash, \
    backslash, semicolon, quote, grave, leftbracket, rightbracket)";

/// The modifier a name stands for under `os`, or `None` if it is not one.
fn modifier(name: &str, os: Os) -> Option<&'static str> {
    Some(match (name, os) {
        // `mod` means "the shortcut key of this platform": copy is mod+c.
        ("mod" | "cmdorctrl" | "commandorcontrol" | "primary", Os::Mac) => "cmd",
        ("mod" | "cmdorctrl" | "commandorcontrol" | "primary", Os::Linux) => "ctrl",
        // macOS has no Super key, so the spellings that mean it there mean
        // Command, as they always have.
        ("cmd" | "command" | "meta" | "super" | "win" | "windows", Os::Mac) => "cmd",
        // On Linux the copy key is Ctrl+C, so an agent that learned `cmd+c`
        // means copy; `super` and friends name the Super key itself.
        ("cmd" | "command" | "ctrl" | "control", Os::Linux) => "ctrl",
        ("super" | "meta" | "win" | "windows", Os::Linux) => "super",
        ("ctrl" | "control", Os::Mac) => "ctrl",
        ("shift", _) => "shift",
        ("alt" | "opt" | "option", Os::Mac) => "opt",
        ("alt" | "opt" | "option", Os::Linux) => "alt",
        // No keysym on Linux and a hardware modifier on laptops; the backends
        // accept and ignore it there, as before.
        ("fn", _) => "fn",
        _ => return None,
    })
}

/// A single punctuation character's key name.
fn punctuation_name(c: char) -> Option<&'static str> {
    Some(match c {
        '-' => "minus",
        '=' => "equal",
        '+' => "plus",
        ',' => "comma",
        '.' => "period",
        '/' => "slash",
        '\\' => "backslash",
        ';' => "semicolon",
        '\'' => "quote",
        '`' => "grave",
        '[' => "leftbracket",
        ']' => "rightbracket",
        _ => return None,
    })
}

/// Shifted US-layout symbols are kept as the character itself; the macOS table
/// knows which key and Shift they take, and a Linux keysym needs no layout.
const SHIFTED: &str = "!@#$%^&*()_{}|:\"<>?~";

/// Canonical name for a key spelled by one character.
fn char_key(c: char) -> Option<String> {
    if let Some(n) = punctuation_name(c) {
        return Some(n.to_string());
    }
    if c.is_ascii_alphanumeric() {
        return Some(c.to_ascii_lowercase().to_string());
    }
    if SHIFTED.contains(c) {
        return Some(c.to_string());
    }
    // Non-ASCII letters (`é`) are typeable on Linux through their keysym; the
    // macOS table will say if it has no keycode for one.
    if c.is_alphanumeric() {
        return Some(c.to_lowercase().collect());
    }
    None
}

/// Canonical name for a key spelled with a word.
fn named_key(word: &str) -> Option<String> {
    // `Page Up`, `page_up`, `Page-Up` and `PageUp` are one spelling.
    let k: String = word
        .chars()
        .filter(|c| !matches!(c, ' ' | '_' | '-'))
        .collect::<String>()
        .to_lowercase();
    let canonical = match k.as_str() {
        "return" | "enter" | "ret" => "return",
        "tab" => "tab",
        "space" | "spacebar" => "space",
        // `delete` is the Mac name for the key above Return, which erases
        // backwards; `del` and `forwarddelete` erase forwards.
        "delete" | "backspace" | "bksp" => "delete",
        "forwarddelete" | "forwarddel" | "fwddelete" | "del" => "forwarddelete",
        "escape" | "esc" => "escape",
        "left" | "arrowleft" | "leftarrow" => "left",
        "right" | "arrowright" | "rightarrow" => "right",
        "up" | "arrowup" | "uparrow" => "up",
        "down" | "arrowdown" | "downarrow" => "down",
        "home" => "home",
        "end" => "end",
        "pageup" | "pgup" | "prior" => "pageup",
        "pagedown" | "pgdn" | "pgdown" | "next" => "pagedown",
        "insert" | "ins" | "help" => "insert",
        "capslock" => "capslock",
        "printscreen" | "print" | "prtsc" | "prtscr" => "printscreen",
        "minus" | "dash" | "hyphen" => "minus",
        "equal" | "equals" => "equal",
        "plus" => "plus",
        "comma" => "comma",
        "period" | "dot" | "fullstop" => "period",
        "slash" | "forwardslash" => "slash",
        "backslash" => "backslash",
        "semicolon" => "semicolon",
        "quote" | "apostrophe" => "quote",
        "grave" | "backtick" => "grave",
        "leftbracket" | "openbracket" | "lbracket" => "leftbracket",
        "rightbracket" | "closebracket" | "rbracket" => "rightbracket",
        _ => {
            // F1 to F20; nothing between `f` and the number.
            if let Some(n) = k.strip_prefix('f').and_then(|d| d.parse::<u8>().ok()) {
                if (1..=20).contains(&n) && k.len() == 1 + n.to_string().len() {
                    return Some(format!("f{n}"));
                }
            }
            return None;
        }
    };
    Some(canonical.to_string())
}

/// Canonical key name for one token, or `None` if it names no key.
fn key_name(token: &str) -> Option<String> {
    let mut chars = token.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => char_key(c),
        (Some(_), Some(_)) => named_key(token),
        _ => None,
    }
}

/// Split a combo into tokens. `+` separates unless there is none and a `-` is
/// present (`ctrl-a`); a separator that ends the string stands for the key of
/// that name (`cmd++`, `ctrl--`).
fn tokenize(s: &str) -> Result<(Vec<String>, char), String> {
    let sep = if s.contains('+') {
        '+'
    } else if s.chars().count() > 1 && s.contains('-') {
        '-'
    } else {
        return Ok((vec![s.to_string()], '+'));
    };
    let (body, literal) = if s == sep.to_string() {
        ("", true)
    } else if s.ends_with(sep) && s[..s.len() - 1].ends_with(sep) {
        (&s[..s.len() - 2], true)
    } else {
        (s, false)
    };
    let mut tokens: Vec<String> = Vec::new();
    if !body.is_empty() {
        for t in body.split(sep) {
            let t = t.trim();
            if t.is_empty() {
                return Err(format!(
                    "'{s}' has an empty segment; a combo is modifiers then one key, e.g. cmd{sep}s \
                     (the {} key itself is written '{}')",
                    if sep == '+' { "plus" } else { "minus" },
                    if sep == '+' { "plus" } else { "minus" },
                ));
            }
            tokens.push(t.to_string());
        }
    }
    if literal {
        tokens.push(sep.to_string());
    }
    Ok((tokens, sep))
}

/// Parse any reasonable spelling of a key combo.
///
/// Case does not matter. Exactly one non-modifier key is required: a second one
/// is an error, not a silent drop, because pressing only half of what was asked
/// for looks like success.
pub fn parse_combo(input: &str, os: Os) -> Result<Combo, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("empty combo".into());
    }
    let (tokens, sep) = tokenize(s)?;
    let mut modifiers: Vec<&'static str> = Vec::new();
    let mut keys: Vec<&str> = Vec::new();
    for t in &tokens {
        match modifier(&t.to_lowercase(), os) {
            Some(m) => {
                if !modifiers.contains(&m) {
                    modifiers.push(m);
                }
            }
            None => keys.push(t),
        }
    }
    let key = match keys.as_slice() {
        [] => {
            return Err(format!(
                "'{input}' names only modifiers; add the key to press, e.g. cmd+s"
            ))
        }
        [one] => key_name(one).ok_or_else(|| format!("unknown key '{one}'; {KEY_NAMES}"))?,
        many => {
            // `ctrl-page-up` splits into two tokens that are one key's name.
            let joined = many.concat();
            match (sep, key_name(&joined)) {
                ('-', Some(k)) => k,
                _ => {
                    return Err(format!(
                        "'{input}' has {} keys ({}); a combo is modifiers plus exactly one key. \
                         Press keys one after another with separate calls",
                        many.len(),
                        many.join(", ")
                    ))
                }
            }
        }
    };
    Ok(Combo { modifiers, key })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mac(s: &str) -> String {
        parse_combo(s, Os::Mac)
            .unwrap_or_else(|e| panic!("{s}: {e}"))
            .canonical()
    }
    fn linux(s: &str) -> String {
        parse_combo(s, Os::Linux)
            .unwrap_or_else(|e| panic!("{s}: {e}"))
            .canonical()
    }

    #[test]
    fn the_old_vocabulary_still_parses() {
        assert_eq!(mac("cmd+shift+n"), "cmd+shift+n");
        assert_eq!(mac("return"), "return");
        assert_eq!(mac("cmd+1"), "cmd+1");
        assert_eq!(mac("escape"), "escape");
    }

    #[test]
    fn case_does_not_matter() {
        assert_eq!(mac("Control+A"), "ctrl+a");
        assert_eq!(mac("Cmd+Shift+Z"), "cmd+shift+z");
        assert_eq!(mac("CMD+S"), "cmd+s");
        assert_eq!(mac("Return"), "return");
        assert_eq!(linux("Control+A"), "ctrl+a");
    }

    #[test]
    fn a_dash_separates_when_there_is_no_plus() {
        assert_eq!(mac("ctrl-a"), "ctrl+a");
        assert_eq!(mac("Cmd-Shift-Z"), "cmd+shift+z");
        assert_eq!(mac("ctrl-page-up"), "ctrl+pageup");
        // A lone dash is the minus key, not an empty combo.
        assert_eq!(mac("-"), "minus");
        assert_eq!(mac("cmd--"), "cmd+minus");
        assert_eq!(mac("cmd+-"), "cmd+minus");
    }

    #[test]
    fn named_keys_have_many_spellings() {
        for s in ["Return", "enter", "RET"] {
            assert_eq!(mac(s), "return", "{s}");
        }
        for s in ["Esc", "escape", "ESCAPE"] {
            assert_eq!(mac(s), "escape", "{s}");
        }
        for s in ["page_up", "PageUp", "pgup", "Page Up", "page-up"] {
            assert_eq!(mac(s), "pageup", "{s}");
        }
        for s in ["page_down", "PageDown", "pgdn"] {
            assert_eq!(mac(s), "pagedown", "{s}");
        }
        for s in ["ArrowUp", "Up", "uparrow"] {
            assert_eq!(mac(s), "up", "{s}");
        }
        assert_eq!(mac("ArrowLeft"), "left");
        for s in ["ForwardDelete", "Del", "fwddelete"] {
            assert_eq!(mac(s), "forwarddelete", "{s}");
        }
        for s in ["Delete", "BackSpace"] {
            assert_eq!(mac(s), "delete", "{s}");
        }
        assert_eq!(mac("Insert"), "insert");
        assert_eq!(mac("Space"), "space");
        assert_eq!(mac("Tab"), "tab");
    }

    #[test]
    fn function_keys_run_f1_to_f20() {
        assert_eq!(mac("F1"), "f1");
        assert_eq!(mac("cmd+F12"), "cmd+f12");
        assert_eq!(mac("f20"), "f20");
        for bad in ["f0", "f21", "f01", "ff"] {
            assert!(parse_combo(bad, Os::Mac).is_err(), "{bad}");
        }
    }

    #[test]
    fn punctuation_by_character_or_by_name() {
        assert_eq!(mac("cmd+,"), "cmd+comma");
        assert_eq!(mac("cmd+comma"), "cmd+comma");
        assert_eq!(mac("cmd+."), "cmd+period");
        assert_eq!(mac("cmd+/"), "cmd+slash");
        assert_eq!(mac("cmd+\\"), "cmd+backslash");
        assert_eq!(mac("cmd+["), "cmd+leftbracket");
        assert_eq!(mac("cmd+]"), "cmd+rightbracket");
        assert_eq!(mac("cmd+="), "cmd+equal");
        assert_eq!(mac("cmd+;"), "cmd+semicolon");
        assert_eq!(mac("cmd+'"), "cmd+quote");
        assert_eq!(mac("cmd+`"), "cmd+grave");
        assert_eq!(mac("cmd+?"), "cmd+?");
    }

    #[test]
    fn a_literal_plus_key_is_reachable() {
        assert_eq!(mac("cmd+plus"), "cmd+plus");
        assert_eq!(mac("cmd++"), "cmd+plus");
        assert_eq!(mac("ctrl+shift++"), "ctrl+shift+plus");
        assert_eq!(mac("+"), "plus");
        assert_eq!(mac("plus"), "plus");
    }

    #[test]
    fn a_second_key_is_an_error_not_a_silent_drop() {
        let e = parse_combo("a+b", Os::Mac).unwrap_err();
        assert!(e.contains("2 keys") && e.contains("a, b"), "{e}");
        assert!(parse_combo("cmd+a+b", Os::Mac).is_err());
        assert!(parse_combo("return+tab", Os::Linux).is_err());
        // Dash mode has no way to tell `a-b` from a key name, so it errors too.
        assert!(parse_combo("a-b", Os::Mac).is_err());
    }

    #[test]
    fn nothing_to_press_is_an_error() {
        for bad in [
            "",
            "   ",
            "cmd",
            "cmd+shift",
            "cmd+",
            "+a",
            "cmd++a",
            "ctrl-",
        ] {
            assert!(parse_combo(bad, Os::Mac).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn an_unknown_key_lists_what_is_accepted() {
        let e = parse_combo("cmd+nosuchkey", Os::Mac).unwrap_err();
        assert!(e.contains("nosuchkey"), "{e}");
        assert!(e.contains("pageup") && e.contains("f1-f20"), "{e}");
        let e = parse_combo("hyper+a", Os::Mac).unwrap_err();
        assert!(e.contains("hyper"), "{e}");
    }

    #[test]
    fn mod_is_command_on_mac_and_control_on_linux() {
        for m in [
            "mod",
            "Mod",
            "cmdorctrl",
            "CmdOrCtrl",
            "CommandOrControl",
            "primary",
        ] {
            assert_eq!(mac(&format!("{m}+c")), "cmd+c", "{m}");
            assert_eq!(linux(&format!("{m}+c")), "ctrl+c", "{m}");
        }
    }

    #[test]
    fn platform_modifier_meanings_hold() {
        // Linux: cmd is how the shortcut key was learned; super is the Super key.
        assert_eq!(linux("cmd+c"), "ctrl+c");
        assert_eq!(linux("super+h"), "super+h");
        assert_eq!(linux("alt+f4"), "alt+f4");
        assert_eq!(linux("opt+f4"), "alt+f4");
        // cmd and ctrl together collapse to one Control press.
        assert_eq!(linux("cmd+ctrl+a"), "ctrl+a");
        // Mac: alt is Option; meta and super have always meant Command.
        assert_eq!(mac("alt+tab"), "opt+tab");
        assert_eq!(mac("meta+up"), "cmd+up");
        assert_eq!(mac("super+space"), "cmd+space");
        assert_eq!(mac("fn+f1"), "fn+f1");
    }

    #[test]
    fn modifiers_dedupe_and_keep_their_order() {
        assert_eq!(mac("shift+cmd+shift+a"), "shift+cmd+a");
    }

    #[test]
    fn spaces_around_segments_are_tolerated() {
        assert_eq!(mac(" Control + A "), "ctrl+a");
    }

    #[test]
    fn non_ascii_letters_pass_through_for_the_backend_to_judge() {
        assert_eq!(linux("ctrl+é"), "ctrl+é");
    }

    #[test]
    fn os_comes_from_the_backend_platform_name() {
        assert_eq!(Os::from_platform("linux"), Os::Linux);
        assert_eq!(Os::from_platform("macos"), Os::Mac);
        assert_eq!(Os::from_platform("mock"), Os::Mac);
    }

    /// Whatever this emits, a backend must be able to take apart with a plain
    /// split on `+`: no canonical string may contain a stray separator.
    #[test]
    fn canonical_strings_split_cleanly_on_plus() {
        for s in [
            "cmd++",
            "ctrl+shift+plus",
            "cmd+,",
            "Control+A",
            "+",
            "ctrl-page-up",
        ] {
            let c = mac(s);
            let n = c.split('+').count();
            assert_eq!(n, c.matches('+').count() + 1, "{c}");
            assert!(c.split('+').all(|seg| !seg.is_empty()), "{c}");
        }
    }
}
