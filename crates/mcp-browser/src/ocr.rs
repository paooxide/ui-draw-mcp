//! Reading text off a page that has no usable DOM.
//!
//! A canvas app, an image-based UI or an annotation tool shows its text as
//! pixels: `browser_snapshot` sees one `<canvas>` and `browser_query` finds
//! nothing. Until now the only way through was a screenshot the model read
//! with its own eyes, every turn, guessing coordinates off the picture. This
//! runs the capture `browser_screenshot` already has through the recogniser
//! and hands back each line with a box in the **viewport CSS pixels**
//! `browser_act` takes as `x`/`y`: the browser-side twin of `ocr_region`.
//!
//! The recogniser and the text search live in `mcp-vision` and are shared
//! with the desktop tool, so an agent that knows one knows the other. What is
//! here is the part only a browser needs: the image-pixel to CSS-pixel mapping
//! and the result shape.

use mcp_vision::find::{normalize, Found};
use mcp_vision::OcrLine;
use serde_json::{json, Value};

/// How the capture relates to the page: what the grid works out, reused here
/// so the two agree about where a pixel is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    /// Image pixels per CSS pixel: the device scale factor, or for a viewport
    /// shot the image width over the viewport width (the same number unless
    /// the page zoomed between measure and capture).
    pub scale: f64,
    /// CSS-px position in the viewport of the image's top-left corner: `(0,0)`
    /// for a viewport shot, the element's rect for a `ref` shot.
    pub origin: (f64, f64),
    /// `"viewport"` when `origin` is known, `"element"` when a `ref` shot's
    /// rect could not be read and boxes are offsets inside the element.
    pub space: &'static str,
    /// The viewport in CSS px, as the page sees it.
    pub viewport: (f64, f64),
}

/// How many `find` hits are listed; the rest are counted.
pub const MAX_FIND_MATCHES: usize = 10;
/// How many lines a no-match hint names.
const NEARBY: usize = 5;

/// Map an image-pixel box to viewport CSS px.
///
/// The picture is `scale` device pixels per CSS pixel, so on a 2x display an
/// OCR box at pixel x=400 is at CSS x=200; passing the pixel to `browser_act`
/// would click twice as far right as the text. An element shot starts at the
/// element, so its position is added back.
pub fn px_to_css(px: (f64, f64, f64, f64), origin: (f64, f64), scale: f64) -> (f64, f64, f64, f64) {
    let s = if scale > 0.0 { scale } else { 1.0 };
    (origin.0 + px.0 / s, origin.1 + px.1 / s, px.2 / s, px.3 / s)
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn bounds_json((x, y, w, h): (f64, f64, f64, f64)) -> Value {
    json!({ "x": round1(x), "y": round1(y), "w": round1(w), "h": round1(h) })
}

/// What every OCR result says about the capture, so a box can be trusted.
fn common(g: &Geometry, lines: usize, img: (u32, u32)) -> Value {
    json!({
        "width": img.0,
        "height": img.1,
        "scale": g.scale,
        "coordinate_space": g.space,
        "origin": { "x": g.origin.0, "y": g.origin.1 },
        "viewport": { "w": g.viewport.0, "h": g.viewport.1 },
        "line_count": lines,
        "engine": mcp_vision::ocr::ENGINE,
    })
}

/// The `ocr: true` result: every recognised line, in reading order, with its
/// box and centre in CSS px.
pub fn lines_result(lines: &[OcrLine], g: &Geometry, img: (u32, u32)) -> Value {
    let items: Vec<Value> = lines
        .iter()
        .map(|l| {
            let b = px_to_css(l.px, g.origin, g.scale);
            json!({
                "text": l.text,
                "confidence": l.confidence,
                "bounds": bounds_json(b),
                // Pre-computed because it is what a click needs, and computing
                // it from bounds is a step an agent can get wrong.
                "center": { "x": round1(b.0 + b.2 / 2.0), "y": round1(b.1 + b.3 / 2.0) },
            })
        })
        .collect();
    let text = lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let mut data = common(g, lines.len(), img);
    data["text"] = json!(text);
    data["lines"] = json!(items);
    data["ocr_note"] = json!(if g.space == "viewport" {
        "bounds and center are viewport CSS px: pass center x,y to browser_act with no ref or \
         query to click the text. The engine reports no per-line confidence (always 1)."
    } else {
        "bounds and center are CSS px from the element's top-left (its viewport position could \
         not be read): pass them as x,y together with the same ref."
    });
    data
}

/// The `find` result: the hits, best first, each with the CSS point to click,
/// instead of the page of text around them. No hit is still `ok`, with
/// `count: 0` and a hint naming the lines that come closest.
pub fn find_result(
    query: &str,
    exact: bool,
    found: &[Found],
    lines: &[OcrLine],
    g: &Geometry,
    img: (u32, u32),
) -> Value {
    let matches: Vec<Value> = found
        .iter()
        .take(MAX_FIND_MATCHES)
        .enumerate()
        .map(|(rank, f)| {
            let line = &lines[f.line];
            let b = px_to_css(f.px, g.origin, g.scale);
            json!({
                "rank": rank + 1,
                "text": line.text,
                "matched": f.kind.as_str(),
                "confidence": line.confidence,
                // Pre-computed for browser_act, as `center` is on a full read.
                "x": round1(b.0 + b.2 / 2.0),
                "y": round1(b.1 + b.3 / 2.0),
                "bounds": bounds_json(b),
            })
        })
        .collect();
    let mut data = common(g, lines.len(), img);
    data["find"] = json!(query);
    data["exact"] = json!(exact);
    data["matches"] = json!(matches);
    data["count"] = json!(found.len());
    data["note"] = json!(if g.space == "viewport" {
        "x,y is the centre of the match in viewport CSS px: pass it to browser_act with no ref \
         or query. When only part of a line matched, the box is estimated from the character \
         positions (the recogniser reports lines, not words)."
    } else {
        "x,y is the centre of the match in CSS px from the element's top-left (its viewport \
         position could not be read): pass it as x,y together with the same ref."
    });
    if found.is_empty() {
        data["hint"] = json!(no_match_hint(query, exact, lines));
    }
    data
}

/// Why nothing matched, and what is on the page instead: the few recognised
/// lines closest to the query, so a misread ("Sove") or a different spelling
/// is one glance away rather than another call.
pub fn no_match_hint(query: &str, exact: bool, lines: &[OcrLine]) -> String {
    if lines.is_empty() {
        return "no text was recognised in the capture; the page may still be drawing, or the \
                text may be too small to read: retry after browser_wait, or take a screenshot \
                with grid=true and read the position off the picture"
            .into();
    }
    let near = nearby_lines(lines, query, NEARBY);
    let mut hint = format!(
        "no recognised line contains {query:?} among {} lines read; nearest: {}.",
        lines.len(),
        near.iter()
            .map(|t| format!("{t:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    hint.push_str(" Try a shorter or differently spelled query");
    if exact {
        hint.push_str(", drop 'exact'");
    }
    hint.push_str(", or pass ocr=true for every line with its position.");
    hint
}

/// The `n` lines most like `query`: by edit distance between the normalised
/// query and the closest same-length window of the line, so a long sentence
/// that contains a near miss ranks with a short label that is one; at a tie
/// the shorter line first (a label before a sentence), then reading order.
pub fn nearby_lines(lines: &[OcrLine], query: &str, n: usize) -> Vec<String> {
    let q: Vec<char> = normalize(query).chars().collect();
    let mut scored: Vec<(usize, usize, usize, &str)> = lines
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let t = normalize(&l.text);
            (
                window_distance(&t, &q),
                t.chars().count(),
                i,
                l.text.as_str(),
            )
        })
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    scored
        .into_iter()
        .take(n)
        .map(|(_, _, _, t)| t.to_string())
        .collect()
}

/// Smallest Levenshtein distance between `q` and any window of `text` about
/// as long as `q` (a short line is compared whole).
fn window_distance(text: &str, q: &[char]) -> usize {
    let t: Vec<char> = text.chars().collect();
    if t.len() <= q.len() {
        return levenshtein(&t, q);
    }
    (0..=t.len() - q.len())
        .map(|s| levenshtein(&t[s..s + q.len()], q))
        .min()
        .unwrap_or(usize::MAX)
}

fn levenshtein(a: &[char], b: &[char]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_vision::find::find_matches;

    fn line(text: &str, px: (f64, f64, f64, f64)) -> OcrLine {
        OcrLine {
            text: text.into(),
            confidence: 1.0,
            px,
        }
    }

    fn geometry(scale: f64, origin: (f64, f64)) -> Geometry {
        Geometry {
            scale,
            origin,
            space: "viewport",
            viewport: (900.0, 700.0),
        }
    }

    /// The mapping that makes a box clickable. On a 2x display the image has
    /// twice the pixels of the viewport, so a pixel box must be halved before
    /// it is a `browser_act` point.
    #[test]
    fn a_2x_capture_maps_pixels_to_half_as_many_css_px() {
        let (x, y, w, h) = px_to_css((400.0, 200.0, 100.0, 40.0), (0.0, 0.0), 2.0);
        assert_eq!((x, y, w, h), (200.0, 100.0, 50.0, 20.0));
    }

    #[test]
    fn a_1x_capture_maps_one_to_one() {
        assert_eq!(
            px_to_css((10.0, 20.0, 30.0, 40.0), (0.0, 0.0), 1.0),
            (10.0, 20.0, 30.0, 40.0)
        );
    }

    /// An element shot starts at the element: its viewport position is added
    /// back so the point is where the text is on the page, not in the crop.
    #[test]
    fn an_element_shot_adds_the_elements_viewport_position() {
        let (x, y, w, h) = px_to_css((40.0, 20.0, 80.0, 20.0), (230.0, 130.0), 2.0);
        assert_eq!((x, y, w, h), (250.0, 140.0, 40.0, 10.0));
    }

    #[test]
    fn a_zero_scale_does_not_divide_by_zero() {
        assert_eq!(
            px_to_css((10.0, 10.0, 10.0, 10.0), (0.0, 0.0), 0.0),
            (10.0, 10.0, 10.0, 10.0)
        );
    }

    /// The full read: every line with a CSS-px box and a pre-computed centre,
    /// and the capture facts needed to trust them.
    #[test]
    fn lines_result_reports_css_boxes_centres_and_the_scale() {
        let lines = [line("Save", (400.0, 200.0, 100.0, 40.0))];
        let d = lines_result(&lines, &geometry(2.0, (0.0, 0.0)), (1800, 1400));
        assert_eq!(d["line_count"], 1);
        assert_eq!(d["scale"], 2.0);
        assert_eq!(d["coordinate_space"], "viewport");
        assert_eq!(d["engine"], "ocrs");
        assert_eq!(d["text"], "Save");
        assert_eq!(d["width"], 1800);
        let l = &d["lines"][0];
        assert_eq!(
            l["bounds"],
            json!({ "x": 200.0, "y": 100.0, "w": 50.0, "h": 20.0 })
        );
        assert_eq!(l["center"], json!({ "x": 225.0, "y": 110.0 }));
    }

    /// `find` ranks like `ocr_region`, and each hit carries the click point.
    #[test]
    fn find_result_lists_ranked_hits_with_click_points() {
        let lines = [
            line("Click Save to continue", (0.0, 0.0, 440.0, 40.0)),
            line("Save", (400.0, 200.0, 100.0, 40.0)),
        ];
        let found = find_matches(&lines, "save", false);
        let d = find_result(
            "save",
            false,
            &found,
            &lines,
            &geometry(2.0, (0.0, 0.0)),
            (1800, 1400),
        );
        assert_eq!(d["count"], 2);
        assert_eq!(d["line_count"], 2);
        assert!(d.get("hint").is_none());
        let m = &d["matches"];
        assert_eq!(m[0]["rank"], 1);
        assert_eq!(m[0]["text"], "Save");
        assert_eq!(m[0]["matched"], "line");
        assert_eq!(
            (m[0]["x"].as_f64(), m[0]["y"].as_f64()),
            (Some(225.0), Some(110.0))
        );
        assert_eq!(m[1]["matched"], "word");
        assert_eq!(m[1]["text"], "Click Save to continue");
    }

    /// No hit is not an error: `ok` with `count: 0` and the nearest lines, so
    /// a misread is one glance away rather than another call.
    #[test]
    fn no_match_is_count_zero_with_nearby_lines() {
        let lines = [
            line("Cancel", (0.0, 0.0, 60.0, 20.0)),
            line("Sove", (0.0, 30.0, 60.0, 20.0)),
            line("Open file", (0.0, 60.0, 60.0, 20.0)),
        ];
        let d = find_result(
            "Save",
            true,
            &[],
            &lines,
            &geometry(1.0, (0.0, 0.0)),
            (900, 700),
        );
        assert_eq!(d["count"], 0);
        assert_eq!(d["matches"], json!([]));
        let hint = d["hint"].as_str().unwrap();
        assert!(
            hint.starts_with("no recognised line contains \"Save\" among 3 lines"),
            "{hint}"
        );
        assert!(hint.contains("\"Sove\", \"Cancel\""), "{hint}");
        assert!(hint.contains("drop 'exact'"), "{hint}");
    }

    #[test]
    fn no_text_at_all_says_so() {
        let hint = no_match_hint("Save", false, &[]);
        assert!(hint.starts_with("no text was recognised"), "{hint}");
    }

    /// A long sentence holding a near miss ("Sove", one edit away) ranks with
    /// a short near-miss label, since the comparison is against the closest
    /// window, not the whole line; at a tie the label comes first. "Something"
    /// shares an "s..e" window two edits away, so it beats "Cancel".
    #[test]
    fn nearby_lines_rank_by_closest_window_then_shortest() {
        let lines = [
            line("Please press Sove now", (0.0, 0.0, 1.0, 1.0)),
            line("Something else entirely", (0.0, 0.0, 1.0, 1.0)),
            line("Sav", (0.0, 0.0, 1.0, 1.0)),
            line("Cancel", (0.0, 0.0, 1.0, 1.0)),
        ];
        let near = nearby_lines(&lines, "save", 3);
        assert_eq!(
            near,
            ["Sav", "Please press Sove now", "Something else entirely"]
        );
    }

    #[test]
    fn levenshtein_is_the_edit_distance() {
        let c = |s: &str| s.chars().collect::<Vec<_>>();
        assert_eq!(levenshtein(&c("kitten"), &c("sitting")), 3);
        assert_eq!(levenshtein(&c(""), &c("abc")), 3);
        assert_eq!(levenshtein(&c("same"), &c("same")), 0);
    }
}
