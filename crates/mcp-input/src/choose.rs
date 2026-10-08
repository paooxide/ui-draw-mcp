//! Choosing an option from a popup button or combo box by its text.
//!
//! The pointing-and-pressing is the OS's business and lives in the backends.
//! What is shared is the part that decides *which* item the caller meant and
//! what to say when none fits, so macOS and Linux answer the same way.

/// Most options an error lists; a long list is a wall of text, and the
/// caller needs enough to see what the choices look like, not all of them.
pub const MAX_LISTED: usize = 25;

/// One entry of an open popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionItem {
    pub title: String,
    pub enabled: bool,
}

/// How a wanted option related to the entries on offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    /// Index of an enabled entry.
    Found(usize),
    /// Only disabled entries match; the index of the first.
    Disabled(usize),
    Missing,
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Case-insensitive, whitespace-collapsed, and tolerant of `…` written as
/// `...`: menus use the one character, callers type the three.
fn loose(s: &str) -> String {
    collapse(s).replace('…', "...").to_lowercase()
}

/// Do two texts name the same option? The comparison the read-back uses too.
pub fn same_option(a: &str, b: &str) -> bool {
    loose(a) == loose(b)
}

/// Pick the entry `wanted` names: an exact match, else one that differs only
/// in whitespace, else one that differs only in case. The first tier with a
/// match decides, so `Dark` is never taken for `dark` when both exist.
pub fn pick_option(items: &[OptionItem], wanted: &str) -> Pick {
    let tiers: [&dyn Fn(&str) -> bool; 3] = [
        &|t| t == wanted,
        &|t| collapse(t) == collapse(wanted),
        &|t| loose(t) == loose(wanted),
    ];
    for tier in tiers {
        let hits: Vec<usize> = (0..items.len())
            .filter(|&i| tier(&items[i].title))
            .collect();
        if let Some(&i) = hits.iter().find(|&&i| items[i].enabled) {
            return Pick::Found(i);
        }
        if let Some(&i) = hits.first() {
            return Pick::Disabled(i);
        }
    }
    Pick::Missing
}

/// `"A", "B", ... (3 more)`.
pub fn list_options(items: &[OptionItem]) -> String {
    let shown: Vec<String> = items
        .iter()
        .take(MAX_LISTED)
        .map(|i| format!("{:?}", i.title))
        .collect();
    let mut s = shown.join(", ");
    if items.len() > MAX_LISTED {
        s.push_str(&format!(", ... {} more", items.len() - MAX_LISTED));
    }
    s
}

/// The message for a wanted option that no entry matches.
pub fn missing_message(items: &[OptionItem], wanted: &str) -> String {
    if items.is_empty() {
        format!("no option {wanted:?}: the control offers no options")
    } else {
        format!(
            "no option {wanted:?}; the options are: {}",
            list_options(items)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(titles: &[&str]) -> Vec<OptionItem> {
        titles
            .iter()
            .map(|t| OptionItem {
                title: (*t).into(),
                enabled: true,
            })
            .collect()
    }

    #[test]
    fn exact_beats_whitespace_beats_case() {
        let i = items(&["dark", "Dark", "Dark  Mode"]);
        assert_eq!(pick_option(&i, "Dark"), Pick::Found(1));
        assert_eq!(pick_option(&i, "dark"), Pick::Found(0));
        assert_eq!(pick_option(&i, "Dark Mode"), Pick::Found(2));
        assert_eq!(pick_option(&i, "DARK MODE"), Pick::Found(2));
        assert_eq!(pick_option(&i, "light"), Pick::Missing);
    }

    #[test]
    fn an_ellipsis_may_be_typed_as_three_dots() {
        let i = items(&["Other…", "Rich Text"]);
        assert_eq!(pick_option(&i, "Other..."), Pick::Found(0));
        assert!(same_option("Other…", "other..."));
    }

    #[test]
    fn a_disabled_match_is_reported_as_such() {
        let mut i = items(&["Plain Text", "Rich Text"]);
        i[1].enabled = false;
        assert_eq!(pick_option(&i, "Rich Text"), Pick::Disabled(1));
        // An enabled twin wins over a disabled one.
        i.push(OptionItem {
            title: "Rich Text".into(),
            enabled: true,
        });
        assert_eq!(pick_option(&i, "Rich Text"), Pick::Found(2));
    }

    #[test]
    fn partial_text_is_not_a_match() {
        // A substring would pick the wrong item as often as the right one.
        assert_eq!(pick_option(&items(&["Rich Text"]), "Rich"), Pick::Missing);
    }

    #[test]
    fn the_listing_is_capped() {
        let titles: Vec<String> = (1..=30).map(|n| format!("Option {n}")).collect();
        let refs: Vec<&str> = titles.iter().map(String::as_str).collect();
        let msg = missing_message(&items(&refs), "x");
        assert!(msg.contains("\"Option 25\""), "{msg}");
        assert!(!msg.contains("\"Option 26\""), "{msg}");
        assert!(msg.ends_with("... 5 more"), "{msg}");
        let short = missing_message(&items(&["A", "B"]), "x");
        assert_eq!(short, "no option \"x\"; the options are: \"A\", \"B\"");
        assert!(missing_message(&[], "x").contains("offers no options"));
    }
}
