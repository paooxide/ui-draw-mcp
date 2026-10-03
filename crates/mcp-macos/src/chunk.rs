//! Splitting text into pieces a single keyboard event can carry.
//!
//! `CGEventKeyboardSetUnicodeString` documents that only the first 20 UTF-16
//! code units of a string set on one event are used, so a longer string is cut
//! silently while the call still reports the full count. Text is therefore
//! posted as a sequence of events of at most [`MAX_UNITS`] units each.
//!
//! Two rules decide where a piece may end:
//!
//! * **Never inside a surrogate pair.** Pieces are `&str` slices on `char`
//!   boundaries, and a `char` outside the BMP is counted as its two units.
//! * **Preferably not inside a grapheme cluster.** No grapheme-segmentation
//!   crate is in the dependency tree, so this does not implement UAX #29. It
//!   keeps together a base character and what follows it when that is a
//!   combining mark (an approximation of Unicode general category M, by block:
//!   see [`is_mark`]), a variation selector, an emoji skin-tone modifier, a
//!   zero-width joiner and the character it joins, and a pair of regional
//!   indicators (a flag). Scripts whose clusters need more than this (Hangul
//!   jamo sequences, Indic conjuncts through a virama) are only kept together
//!   as far as their marks are in the table. A single cluster longer than 20
//!   units cannot fit in one event; it is split at `char` boundaries, the one
//!   case where a cluster is broken.
//!
//! Pure logic, so it is tested on any host.

/// Most UTF-16 code units one event can carry.
pub const MAX_UNITS: usize = 20;

const ZWJ: char = '\u{200D}';

/// Whether `c` is a combining mark that must stay with its base. Covers the
/// common blocks rather than the full Unicode general category M table.
fn is_mark(c: char) -> bool {
    matches!(c as u32,
        0x0300..=0x036F      // combining diacritical marks
        | 0x0483..=0x0489    // Cyrillic
        | 0x0591..=0x05BD | 0x05BF | 0x05C1..=0x05C2 | 0x05C4..=0x05C5 | 0x05C7 // Hebrew
        | 0x0610..=0x061A | 0x064B..=0x065F | 0x0670 | 0x06D6..=0x06DC
        | 0x06DF..=0x06E4 | 0x06E7..=0x06E8 | 0x06EA..=0x06ED // Arabic
        | 0x0900..=0x0903 | 0x093A..=0x094F | 0x0951..=0x0957 | 0x0962..=0x0963 // Devanagari
        | 0x0981..=0x0983 | 0x09BC | 0x09BE..=0x09CD // Bengali
        | 0x0E31 | 0x0E34..=0x0E3A | 0x0E47..=0x0E4E // Thai
        | 0x1AB0..=0x1AFF    // combining diacritical marks extended
        | 0x1DC0..=0x1DFF    // combining diacritical marks supplement
        | 0x20D0..=0x20FF    // combining marks for symbols
        | 0x3099..=0x309A    // kana voiced marks
        | 0xFE00..=0xFE0F    // variation selectors
        | 0xFE20..=0xFE2F    // combining half marks
        | 0xE0100..=0xE01EF  // variation selectors supplement
    )
}

fn is_skin_tone(c: char) -> bool {
    matches!(c as u32, 0x1F3FB..=0x1F3FF)
}

fn is_regional_indicator(c: char) -> bool {
    matches!(c as u32, 0x1F1E6..=0x1F1FF)
}

/// Does `c` continue the cluster whose last character was `prev`?
/// `ri_open` is true when the cluster is exactly one regional indicator.
fn extends(prev: char, c: char, ri_open: bool) -> bool {
    is_mark(c)
        || is_skin_tone(c)
        || c == ZWJ
        || prev == ZWJ
        || (ri_open && is_regional_indicator(c))
}

/// Split `text` into clusters, each a slice of it.
fn clusters(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut prev: Option<char> = None;
    let mut cluster_chars = 0usize;
    let mut first = ' ';
    for (i, c) in text.char_indices() {
        if let Some(p) = prev {
            let ri_open = cluster_chars == 1 && is_regional_indicator(first);
            if !extends(p, c, ri_open) {
                out.push(&text[start..i]);
                start = i;
                cluster_chars = 0;
            }
        }
        if cluster_chars == 0 {
            first = c;
        }
        cluster_chars += 1;
        prev = Some(c);
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

fn units(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// Split `text` into slices of at most [`MAX_UNITS`] UTF-16 code units,
/// breaking between clusters where it can. Concatenating the result gives
/// `text` back.
pub fn chunks(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0; // byte offset where the chunk being built begins
    let mut used = 0; // UTF-16 units in it
    let mut pos = 0; // byte offset just past the last cluster consumed
    for cluster in clusters(text) {
        let n = units(cluster);
        if n > MAX_UNITS {
            // Cannot fit in any event: flush, then break it on char boundaries.
            if used > 0 {
                out.push(&text[start..pos]);
            }
            let mut piece_start = pos;
            let mut piece_units = 0;
            for (i, c) in cluster.char_indices() {
                let w = c.len_utf16();
                if piece_units + w > MAX_UNITS {
                    out.push(&text[piece_start..pos + i]);
                    piece_start = pos + i;
                    piece_units = 0;
                }
                piece_units += w;
            }
            pos += cluster.len();
            start = piece_start;
            used = piece_units;
            continue;
        }
        if used + n > MAX_UNITS {
            out.push(&text[start..pos]);
            start = pos;
            used = 0;
        }
        used += n;
        pos += cluster.len();
    }
    if used > 0 {
        out.push(&text[start..pos]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_fit(cs: &[&str]) -> bool {
        cs.iter().all(|c| units(c) <= MAX_UNITS)
    }

    #[test]
    fn empty_text_has_no_chunks() {
        assert!(chunks("").is_empty());
    }

    #[test]
    fn short_text_is_one_chunk() {
        assert_eq!(chunks("hello"), vec!["hello"]);
    }

    #[test]
    fn ascii_splits_at_exactly_twenty() {
        let s = "a".repeat(20);
        assert_eq!(chunks(&s), vec![s.as_str()]);
        let s = "a".repeat(21);
        let cs = chunks(&s);
        assert_eq!(cs.len(), 2);
        assert_eq!((units(cs[0]), units(cs[1])), (20, 1));
        let s = "b".repeat(200);
        let cs = chunks(&s);
        assert_eq!(cs.len(), 10);
        assert_eq!(cs.concat(), s);
    }

    #[test]
    fn an_emoji_pair_is_never_split() {
        // 19 units, then a 2-unit emoji: it must move whole to the next chunk.
        let s = format!("{}😀", "a".repeat(19));
        let cs = chunks(&s);
        assert_eq!(cs, vec!["a".repeat(19).as_str(), "😀"]);
        // Nothing but emoji: ten fit per chunk, exactly twenty units.
        let s = "😀".repeat(25);
        let cs = chunks(&s);
        assert!(all_fit(&cs));
        assert_eq!(cs.concat(), s);
        assert_eq!(units(cs[0]), 20);
        assert!(cs.iter().all(|c| c.chars().all(|ch| ch == '😀')));
    }

    #[test]
    fn a_combining_mark_stays_with_its_base() {
        // 19 plain units then "e" + U+0301: the pair is 2 units and cannot end
        // the first chunk, so the base moves with its accent.
        let s = format!("{}e\u{0301}", "a".repeat(19));
        let cs = chunks(&s);
        assert_eq!(cs, vec!["a".repeat(19).as_str(), "e\u{0301}"]);
        // Several marks on one base stay together.
        let s = format!("{}a\u{0300}\u{0301}\u{0302}", "x".repeat(18));
        assert_eq!(
            chunks(&s).last().copied(),
            Some("a\u{0300}\u{0301}\u{0302}")
        );
    }

    #[test]
    fn zwj_sequences_and_flags_stay_together() {
        let family = "👨\u{200D}👩\u{200D}👧";
        assert!(units(family) <= MAX_UNITS);
        let s = format!("{}{family}", "a".repeat(15));
        let cs = chunks(&s);
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[1], family);
        // A flag is two regional indicators; two flags are two clusters.
        assert_eq!(clusters("🇯🇵🇺🇸"), vec!["🇯🇵", "🇺🇸"]);
        let s = format!("{}🇯🇵", "a".repeat(17));
        assert_eq!(chunks(&s).last().copied(), Some("🇯🇵"));
    }

    #[test]
    fn an_over_long_cluster_is_split_on_char_boundaries_only() {
        // One base with 30 combining marks: 31 units, cannot fit in one event.
        let s = format!("a{}", "\u{0301}".repeat(30));
        let cs = chunks(&s);
        assert!(all_fit(&cs));
        assert_eq!(cs.concat(), s);
        assert!(cs.len() >= 2);
        // An over-long emoji cluster never splits a surrogate pair.
        let s = "👍🏽\u{200D}".repeat(8);
        let cs = chunks(&s);
        assert!(all_fit(&cs));
        assert_eq!(cs.concat(), s);
    }

    #[test]
    fn mixed_text_round_trips_and_every_chunk_fits() {
        let s = "héllo wörld 😀 日本語のテキスト 🇯🇵 e\u{0301}e\u{0301}e\u{0301} ".repeat(7);
        let cs = chunks(&s);
        assert!(all_fit(&cs));
        assert_eq!(cs.concat(), s);
        assert!(cs.iter().all(|c| !c.is_empty()));
    }
}
