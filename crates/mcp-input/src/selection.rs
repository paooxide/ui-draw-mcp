//! Text-selection arithmetic: which characters a request means, and how the
//! platforms count them.
//!
//! Everything public speaks in characters (Unicode scalar values, `end`
//! exclusive). macOS counts UTF-16 code units, so a character outside the
//! Basic Multilingual Plane is two there; AT-SPI counts characters. Getting
//! the conversion wrong selects the wrong text without any error, so it is
//! pure and tested here rather than inline in a backend.

use crate::backend::Selection;

/// What to select.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RangeSpec {
    /// The whole text.
    All,
    /// Characters `start..end`. `start == end` places the caret.
    Offsets { start: usize, end: usize },
    /// The `occurrence`-th (1-based) match of `needle`.
    Text { needle: String, occurrence: usize },
    /// From the start of the first `from` through the end of the first `to`
    /// that begins at or after the end of that `from`.
    Between { from: String, to: String },
}

/// Characters in `text`.
pub fn char_len(text: &str) -> usize {
    text.chars().count()
}

/// Character offset of byte offset `byte` (which must be on a boundary).
fn chars_before(text: &str, byte: usize) -> usize {
    text[..byte].chars().count()
}

/// Byte ranges of the non-overlapping matches of `needle`, in order.
fn matches_of<'a>(text: &'a str, needle: &'a str) -> impl Iterator<Item = (usize, usize)> + 'a {
    text.match_indices(needle).map(|(i, m)| (i, i + m.len()))
}

/// The character range a spec means in `text`, or why it means none.
pub fn resolve(text: &str, spec: &RangeSpec) -> Result<(usize, usize), String> {
    let total = char_len(text);
    match spec {
        RangeSpec::All => Ok((0, total)),
        RangeSpec::Offsets { start, end } => {
            if start > end {
                return Err(format!("start {start} is after end {end}"));
            }
            if *end > total {
                return Err(format!(
                    "range {start}..{end} runs past the text, which is {total} characters long"
                ));
            }
            Ok((*start, *end))
        }
        RangeSpec::Text { needle, occurrence } => {
            if needle.is_empty() {
                return Err("'text' is empty; there is nothing to find".into());
            }
            if *occurrence == 0 {
                return Err("'occurrence' counts from 1".into());
            }
            let found: Vec<(usize, usize)> = matches_of(text, needle).collect();
            match found.get(occurrence - 1) {
                Some(&(b0, b1)) => Ok((chars_before(text, b0), chars_before(text, b1))),
                None if found.is_empty() => Err(format!("{needle:?} is not in the text")),
                None => Err(format!(
                    "{needle:?} occurs {} time{} in the text, not {occurrence}",
                    found.len(),
                    if found.len() == 1 { "" } else { "s" }
                )),
            }
        }
        RangeSpec::Between { from, to } => {
            if from.is_empty() || to.is_empty() {
                return Err("'from_text' and 'to_text' must not be empty".into());
            }
            let Some((f0, f1)) = matches_of(text, from).next() else {
                return Err(format!("{from:?} is not in the text"));
            };
            // Search for the end only after the start has finished, so
            // "from" and "to" never overlap and an end before the start is
            // not silently swapped.
            let Some((_, t1)) = matches_of(&text[f1..], to)
                .next()
                .map(|(a, b)| (f1 + a, f1 + b))
            else {
                return Err(format!("{to:?} does not follow {from:?} in the text"));
            };
            Ok((chars_before(text, f0), chars_before(text, t1)))
        }
    }
}

/// Characters `start..end` of `text`.
pub fn char_slice(text: &str, start: usize, end: usize) -> String {
    text.chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect()
}

/// A character range as UTF-16 `(location, length)`. `None` when it runs past
/// the text.
pub fn char_range_to_utf16(text: &str, start: usize, end: usize) -> Option<(usize, usize)> {
    if start > end {
        return None;
    }
    let (mut units, mut loc, mut at_end) = (0usize, None, None);
    for (i, c) in text.chars().enumerate() {
        if i == start {
            loc = Some(units);
        }
        if i == end {
            at_end = Some(units);
            break;
        }
        units += c.len_utf16();
    }
    let total = char_len(text);
    if start == total {
        loc = Some(units);
    }
    if end == total && at_end.is_none() {
        at_end = Some(units);
    }
    let (loc, stop) = (loc?, at_end?);
    Some((loc, stop - loc))
}

/// A UTF-16 `(location, length)` as a character range. `None` when it runs
/// past the text or an end falls between the two halves of a surrogate pair,
/// which no character range can express.
pub fn utf16_range_to_char(text: &str, location: usize, length: usize) -> Option<(usize, usize)> {
    let stop = location.checked_add(length)?;
    let (mut units, mut start, mut end) = (0usize, None, None);
    for (i, c) in text.chars().enumerate() {
        if units == location {
            start = Some(i);
        }
        if units == stop {
            end = Some(i);
        }
        units += c.len_utf16();
    }
    let total = char_len(text);
    if units == location {
        start = Some(total);
    }
    if units == stop {
        end = Some(total);
    }
    let (s, e) = (start?, end?);
    (s <= e).then_some((s, e))
}

/// Whether the selection an element reports is the one that was asked for.
///
/// An element can accept a range and then clamp it, snap it to a word or
/// ignore it, and the API call still succeeds. The only honest `ok` is the
/// range read back equalling the range requested.
pub fn judge_selection(wanted: (usize, usize), got: &Selection) -> Result<(), String> {
    if (got.start, got.end) == wanted {
        return Ok(());
    }
    Err(format!(
        "asked for characters {}..{}, but the element reports {}..{} selected",
        wanted.0, wanted.1, got.start, got.end
    ))
}

/// Most selected characters a result echoes.
pub const ECHO_LIMIT: usize = 2_000;

/// `text` cut to [`ECHO_LIMIT`] characters, and whether it was cut.
pub fn clip_echo(text: &str) -> (String, bool) {
    if char_len(text) <= ECHO_LIMIT {
        (text.to_string(), false)
    } else {
        (text.chars().take(ECHO_LIMIT).collect(), true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // "a", a musical symbol (one character, two UTF-16 units), "e" + combining
    // acute (two characters, two units), "z".
    const MIXED: &str = "a\u{1D11E}e\u{301}z";

    fn sel(start: usize, end: usize) -> Selection {
        Selection {
            start,
            end,
            text: String::new(),
            role: None,
        }
    }

    #[test]
    fn a_character_outside_the_bmp_is_two_utf16_units() {
        assert_eq!(MIXED.chars().count(), 5);
        assert_eq!(MIXED.encode_utf16().count(), 6);
        // Selecting just the symbol: it starts after "a" and spans two units.
        assert_eq!(char_range_to_utf16(MIXED, 1, 2), Some((1, 2)));
        // Everything after it shifts by one unit.
        assert_eq!(char_range_to_utf16(MIXED, 2, 3), Some((3, 1)));
        assert_eq!(char_range_to_utf16(MIXED, 4, 5), Some((5, 1)));
        assert_eq!(char_range_to_utf16(MIXED, 0, 5), Some((0, 6)));
    }

    #[test]
    fn a_combining_mark_is_its_own_character_and_its_own_unit() {
        // "e" then U+0301: two characters, so two offsets, never one glyph.
        assert_eq!(char_range_to_utf16(MIXED, 2, 3), Some((3, 1)));
        assert_eq!(char_range_to_utf16(MIXED, 2, 4), Some((3, 2)));
        assert_eq!(char_range_to_utf16(MIXED, 3, 4), Some((4, 1)));
    }

    #[test]
    fn the_caret_and_the_end_of_the_text_convert() {
        assert_eq!(char_range_to_utf16(MIXED, 2, 2), Some((3, 0)));
        assert_eq!(char_range_to_utf16(MIXED, 5, 5), Some((6, 0)));
        assert_eq!(char_range_to_utf16("", 0, 0), Some((0, 0)));
    }

    #[test]
    fn a_range_past_the_text_or_reversed_does_not_convert() {
        assert_eq!(char_range_to_utf16(MIXED, 0, 6), None);
        assert_eq!(char_range_to_utf16(MIXED, 6, 6), None);
        assert_eq!(char_range_to_utf16(MIXED, 3, 2), None);
    }

    #[test]
    fn utf16_ranges_come_back_as_characters() {
        assert_eq!(utf16_range_to_char(MIXED, 1, 2), Some((1, 2)));
        assert_eq!(utf16_range_to_char(MIXED, 3, 2), Some((2, 4)));
        assert_eq!(utf16_range_to_char(MIXED, 0, 6), Some((0, 5)));
        assert_eq!(utf16_range_to_char(MIXED, 6, 0), Some((5, 5)));
    }

    #[test]
    fn a_utf16_range_splitting_a_surrogate_pair_is_refused() {
        // Unit 2 is the low half of the symbol.
        assert_eq!(utf16_range_to_char(MIXED, 2, 1), None);
        assert_eq!(utf16_range_to_char(MIXED, 0, 2), None);
        assert_eq!(utf16_range_to_char(MIXED, 5, 2), None);
    }

    #[test]
    fn the_two_conversions_are_inverses_over_every_valid_range() {
        let n = char_len(MIXED);
        for s in 0..=n {
            for e in s..=n {
                let (loc, len) = char_range_to_utf16(MIXED, s, e).unwrap();
                assert_eq!(utf16_range_to_char(MIXED, loc, len), Some((s, e)));
            }
        }
    }

    #[test]
    fn offsets_resolve_and_are_bounds_checked() {
        let all = |s, e| resolve("hello", &RangeSpec::Offsets { start: s, end: e });
        assert_eq!(all(1, 3), Ok((1, 3)));
        assert_eq!(all(5, 5), Ok((5, 5)));
        assert!(all(3, 1).unwrap_err().contains("after end"));
        assert!(all(0, 6).unwrap_err().contains("5 characters"));
        assert_eq!(resolve("hello", &RangeSpec::All), Ok((0, 5)));
        assert_eq!(resolve("", &RangeSpec::All), Ok((0, 0)));
    }

    #[test]
    fn text_resolves_to_characters_not_bytes() {
        // Bytes and characters differ before the match: the symbol is four
        // bytes, one character.
        let t = "\u{1D11E}\u{1D11E}ab cd ab";
        let r = |n: &str, o| {
            resolve(
                t,
                &RangeSpec::Text {
                    needle: n.into(),
                    occurrence: o,
                },
            )
        };
        assert_eq!(r("ab", 1), Ok((2, 4)));
        assert_eq!(r("ab", 2), Ok((8, 10)));
        assert!(r("ab", 3).unwrap_err().contains("2 times"));
        assert!(r("zz", 1).unwrap_err().contains("not in the text"));
        assert!(r("", 1).unwrap_err().contains("empty"));
        assert!(r("ab", 0).unwrap_err().contains("from 1"));
    }

    #[test]
    fn matching_is_exact_and_does_not_overlap() {
        let r = |t: &str, n: &str, o| {
            resolve(
                t,
                &RangeSpec::Text {
                    needle: n.into(),
                    occurrence: o,
                },
            )
        };
        assert!(r("Hello", "hello", 1).is_err());
        assert_eq!(r("aaaa", "aa", 2), Ok((2, 4)));
        assert!(r("aaa", "aa", 2).is_err());
        // A decomposed "e" + accent does not match the precomposed character.
        assert!(r("caf\u{e9}", "e\u{301}", 1).is_err());
    }

    #[test]
    fn between_runs_from_the_first_start_to_the_end_of_the_next_end() {
        let t = "<<a>> then <<b>>";
        let b = |f: &str, to: &str| {
            resolve(
                t,
                &RangeSpec::Between {
                    from: f.into(),
                    to: to.into(),
                },
            )
        };
        assert_eq!(b("<<", ">>"), Ok((0, 5)));
        assert_eq!(b("then", ">>"), Ok((6, 16)));
        assert!(b(">>", "<<a").unwrap_err().contains("does not follow"));
        assert!(b("nope", ">>").unwrap_err().contains("not in the text"));
        assert!(b("", ">>").is_err());
    }

    #[test]
    fn slices_are_by_character() {
        assert_eq!(char_slice(MIXED, 1, 3), "\u{1D11E}e");
        assert_eq!(char_slice(MIXED, 3, 3), "");
        assert_eq!(char_slice(MIXED, 4, 99), "z");
    }

    #[test]
    fn a_selection_that_is_not_the_one_asked_for_is_not_ok() {
        assert!(judge_selection((2, 5), &sel(2, 5)).is_ok());
        let e = judge_selection((2, 5), &sel(0, 5)).unwrap_err();
        assert!(e.contains("2..5") && e.contains("0..5"), "{e}");
    }

    #[test]
    fn a_long_selection_is_clipped_for_the_result() {
        let (short, cut) = clip_echo("abc");
        assert_eq!((short.as_str(), cut), ("abc", false));
        let long = "x".repeat(ECHO_LIMIT + 5);
        let (clipped, cut) = clip_echo(&long);
        assert_eq!(char_len(&clipped), ECHO_LIMIT);
        assert!(cut);
    }
}
