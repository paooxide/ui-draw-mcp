//! Picking the line a caller asked for out of an OCR result.
//!
//! "Click the Save label in a custom-drawn app" should cost two small calls,
//! not a page of recognised text the model then has to search. This is the
//! search, kept pure so it is tested on synthetic OCR output.

use crate::backend::OcrLine;

/// How a line matched, best first. The order is the ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MatchKind {
    /// The whole line is the query ("Save").
    Whole,
    /// The query is whole words inside the line ("Save" in "Save As").
    Word,
    /// The query is inside a word ("Save" in "Autosave").
    Substring,
}

impl MatchKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MatchKind::Whole => "line",
            MatchKind::Word => "word",
            MatchKind::Substring => "substring",
        }
    }
}

/// One hit: which line, how it matched, and where the matched text is.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    /// Index into the lines that were searched.
    pub line: usize,
    pub kind: MatchKind,
    /// `(x, y, w, h)` in image pixels of the matched text. For a partial match
    /// this is the line's box cut down in proportion to the characters
    /// matched: the recogniser reports lines, not words, so it is an estimate.
    pub px: (f64, f64, f64, f64),
}

/// Lower-case and collapse runs of whitespace to one space, so "  Save   As "
/// and "save as" compare equal.
pub fn normalize(s: &str) -> String {
    s.split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The lines that contain `query`, best first.
///
/// Case-insensitive and whitespace-collapsed. With `exact` only a line that is
/// the query is returned. Otherwise any line containing it is, ranked: whole
/// line, then whole words, then a fragment of a word; within a rank the line
/// with the least else on it first (a button label before a sentence that
/// mentions it), then the more confident read, then reading order.
pub fn find_matches(lines: &[OcrLine], query: &str, exact: bool) -> Vec<Found> {
    let q: Vec<char> = normalize(query).chars().collect();
    if q.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<(Found, usize, f64)> = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let t: Vec<char> = normalize(&l.text).chars().collect();
        if t.len() < q.len() {
            continue;
        }
        if t == q {
            hits.push((
                Found {
                    line: i,
                    kind: MatchKind::Whole,
                    px: l.px,
                },
                0,
                l.confidence,
            ));
            continue;
        }
        if exact {
            continue;
        }
        let Some((start, kind)) = locate(&t, &q) else {
            continue;
        };
        let (x, y, w, h) = l.px;
        let n = t.len() as f64;
        hits.push((
            Found {
                line: i,
                kind,
                px: (x + w * start as f64 / n, y, w * q.len() as f64 / n, h),
            },
            t.len() - q.len(),
            l.confidence,
        ));
    }
    hits.sort_by(|a, b| {
        a.0.kind
            .cmp(&b.0.kind)
            .then(a.1.cmp(&b.1))
            .then(b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.0.line.cmp(&b.0.line))
    });
    hits.into_iter().map(|(f, _, _)| f).collect()
}

/// The first place `q` occurs in `t` as whole words, else the first place it
/// occurs at all.
fn locate(t: &[char], q: &[char]) -> Option<(usize, MatchKind)> {
    let word = |c: char| c.is_alphanumeric();
    let mut first = None;
    for s in 0..=t.len() - q.len() {
        if t[s..s + q.len()] != *q {
            continue;
        }
        first.get_or_insert(s);
        let before = s == 0 || !word(t[s - 1]);
        let after = s + q.len() == t.len() || !word(t[s + q.len()]);
        if before && after {
            return Some((s, MatchKind::Word));
        }
    }
    first.map(|s| (s, MatchKind::Substring))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str, conf: f64, x: f64, y: f64, w: f64) -> OcrLine {
        OcrLine {
            text: text.into(),
            confidence: conf,
            px: (x, y, w, 20.0),
        }
    }

    fn texts<'a>(lines: &'a [OcrLine], found: &[Found]) -> Vec<&'a str> {
        found.iter().map(|f| lines[f.line].text.as_str()).collect()
    }

    #[test]
    fn matching_ignores_case_and_runs_of_whitespace() {
        let lines = [line("  Save   As ", 0.9, 0.0, 0.0, 100.0)];
        assert_eq!(find_matches(&lines, "save as", false).len(), 1);
        assert_eq!(find_matches(&lines, "SAVE\tAS", false).len(), 1);
        assert!(find_matches(&lines, "saveas", false).is_empty());
        assert!(find_matches(&lines, "   ", false).is_empty());
    }

    #[test]
    fn a_label_ranks_before_a_sentence_that_mentions_it() {
        let lines = [
            line("Click Save to keep your changes", 0.99, 0.0, 0.0, 300.0),
            line("Autosave", 0.99, 0.0, 30.0, 80.0),
            line("Save", 0.80, 0.0, 60.0, 40.0),
            line("Save As", 0.95, 0.0, 90.0, 70.0),
        ];
        let found = find_matches(&lines, "save", false);
        assert_eq!(
            texts(&lines, &found),
            [
                "Save",
                "Save As",
                "Click Save to keep your changes",
                "Autosave"
            ]
        );
        let kinds: Vec<_> = found.iter().map(|f| f.kind).collect();
        assert_eq!(
            kinds,
            [
                MatchKind::Whole,
                MatchKind::Word,
                MatchKind::Word,
                MatchKind::Substring
            ]
        );
    }

    #[test]
    fn exact_keeps_only_a_line_that_is_the_query() {
        let lines = [
            line("Save As", 0.9, 0.0, 0.0, 70.0),
            line("save", 0.9, 0.0, 30.0, 40.0),
        ];
        let found = find_matches(&lines, "Save", true);
        assert_eq!(texts(&lines, &found), ["save"]);
    }

    #[test]
    fn equal_matches_prefer_confidence_then_reading_order() {
        let lines = [
            line("OK", 0.5, 0.0, 0.0, 20.0),
            line("OK", 0.9, 0.0, 30.0, 20.0),
            line("OK", 0.9, 0.0, 60.0, 20.0),
        ];
        let found = find_matches(&lines, "ok", false);
        assert_eq!(found.iter().map(|f| f.line).collect::<Vec<_>>(), [1, 2, 0]);
    }

    #[test]
    fn a_partial_match_box_covers_only_the_matched_characters() {
        // "Save As" is 7 characters across 70 px: "As" starts at 5/7 of it.
        let lines = [line("Save As", 0.9, 100.0, 40.0, 70.0)];
        let found = find_matches(&lines, "as", false);
        let (x, y, w, h) = found[0].px;
        assert!((x - 150.0).abs() < 1e-9, "{x}");
        assert_eq!((y, h), (40.0, 20.0));
        assert!((w - 20.0).abs() < 1e-9, "{w}");
        // A whole-line match keeps the recogniser's own box.
        let whole = find_matches(&lines, "save as", false);
        assert_eq!(whole[0].px, (100.0, 40.0, 70.0, 20.0));
    }

    #[test]
    fn no_match_is_empty() {
        let lines = [line("Cancel", 0.9, 0.0, 0.0, 50.0)];
        assert!(find_matches(&lines, "save", false).is_empty());
        assert!(find_matches(&[], "save", false).is_empty());
    }
}
