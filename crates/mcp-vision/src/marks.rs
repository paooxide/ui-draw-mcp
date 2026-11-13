//! Merging recognised text with an accessibility tree, and numbering what is
//! left over as marks.
//!
//! `observe` reads a window two ways: the accessibility tree, which is exact
//! about what an element is but says nothing for a canvas, a game or a web view
//! that exposes no tree, and OCR, which sees every pixel of text but knows
//! nothing about elements. This module is the join, and it is deliberately
//! pure: rectangles in, rectangles out, in screen coordinates, so every rule
//! can be tested on a synthetic set without a window, a recogniser or a
//! screenshot.
//!
//! A text box that lies on an element the tree already names is attached to that
//! element (so an agent clicks the element, by ref, not the pixel). A box on
//! text the tree already says is a duplicate and is dropped. A box on a
//! secure field is dropped, because what a password field draws is not for
//! anyone to read. What remains has no element, and becomes a numbered mark
//! (`m1`, `m2`...) in reading order, with the screen rectangle `mouse_action`
//! takes.
//!
//! Boxes come from a list of sources. OCR is the only one today; the list is
//! where a local grounding model that proposes boxes for unlabelled icons would
//! be added, with no change to the merge.

use serde::Serialize;

use crate::grid::{decode_rgba, encode_rgba, label_size, Canvas, Rect as PixRect};

/// A rectangle in screen points (or in image pixels, when drawing).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Rect { x, y, w, h }
    }

    pub fn area(&self) -> f64 {
        self.w.max(0.0) * self.h.max(0.0)
    }

    pub fn center(&self) -> (f64, f64) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    pub fn contains_point(&self, (px, py): (f64, f64)) -> bool {
        px >= self.x && px <= self.x + self.w && py >= self.y && py <= self.y + self.h
    }

    /// Area shared with `other`.
    pub fn intersection(&self, other: &Rect) -> f64 {
        let w = (self.x + self.w).min(other.x + other.w) - self.x.max(other.x);
        let h = (self.y + self.h).min(other.y + other.h) - self.y.max(other.y);
        if w > 0.0 && h > 0.0 {
            w * h
        } else {
            0.0
        }
    }

    /// Intersection over union, 0 for rectangles with no area.
    pub fn iou(&self, other: &Rect) -> f64 {
        let i = self.intersection(other);
        let u = self.area() + other.area() - i;
        if u > 0.0 {
            i / u
        } else {
            0.0
        }
    }

    /// The share of this rectangle's own area that lies inside `outer`.
    pub fn fraction_inside(&self, outer: &Rect) -> f64 {
        let a = self.area();
        if a > 0.0 {
            self.intersection(outer) / a
        } else {
            0.0
        }
    }
}

/// What an accessibility element is, as far as the merge cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AxKind {
    /// Something with a ref that can be acted on. Text found on it is
    /// attached to it.
    Control,
    /// Text the tree already carries, with no ref of its own. A box that says
    /// the same thing is a duplicate.
    Text,
    /// A secure field. Whatever is drawn on it is withheld.
    Secure,
}

/// One accessibility element, reduced to what the merge needs.
#[derive(Debug, Clone, PartialEq)]
pub struct AxItem {
    /// The element's ref (`@e12`) for a [`AxKind::Control`].
    pub id: Option<String>,
    pub kind: AxKind,
    pub rect: Rect,
    /// What the tree says is on the element: its name and value.
    pub text: Vec<String>,
}

/// A text box proposed by a source, in screen points.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub text: String,
    pub rect: Rect,
    /// 0.0 to 1.0.
    pub confidence: f64,
}

/// A named list of boxes: `ocr` today.
#[derive(Debug, Clone, PartialEq)]
pub struct BoxSource {
    pub name: String,
    pub boxes: Vec<Candidate>,
}

/// How the merge behaves.
#[derive(Debug, Clone)]
pub struct MergeOpts {
    /// Screen areas that belong to other applications' windows. A box whose
    /// centre lies in one, or that is mostly inside one, is not this window's.
    pub occluders: Vec<Rect>,
    /// Set when the boxes may include other applications' pixels but the
    /// areas are not known: every mark is then flagged unreliable.
    pub unreliable: bool,
    /// Share of a box that must lie inside an element for the box to be on it.
    pub containment: f64,
    /// Or an overlap this good, for a box and an element of about one size.
    pub iou: f64,
    /// Overlap at which two boxes from different sources are one thing.
    pub duplicate_iou: f64,
    /// Most entries (attachments plus marks) reported; the rest are counted.
    pub max_listed: usize,
}

impl Default for MergeOpts {
    fn default() -> Self {
        MergeOpts {
            occluders: Vec::new(),
            unreliable: false,
            containment: 0.6,
            iou: 0.4,
            duplicate_iou: 0.6,
            max_listed: 60,
        }
    }
}

/// Text found on an element.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Attached {
    /// The element's ref.
    #[serde(rename = "ref")]
    pub id: String,
    pub text: String,
    pub rect: Rect,
    pub confidence: f64,
    pub source: String,
    /// The tree already says this text; the box is here for its position.
    pub same_as_ax: bool,
}

/// Text with no element: a numbered target for `mouse_action`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Mark {
    pub id: String,
    pub text: String,
    pub rect: Rect,
    pub confidence: f64,
    pub source: String,
    /// The box may belong to a window in front of this one.
    pub unreliable: bool,
}

/// What the merge did with the boxes it was given, so nothing vanishes
/// without being counted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct MergeStats {
    pub boxes_in: usize,
    /// Two boxes for one thing, from different sources or read twice.
    pub duplicates: usize,
    /// Text the tree already carries.
    pub same_as_ax: usize,
    /// On a secure field.
    pub secure_withheld: usize,
    /// In an area another application covers.
    pub covered_dropped: usize,
    /// Empty text, or no area.
    pub empty_dropped: usize,
    /// Left out by `max_listed`.
    pub omitted: usize,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Merged {
    pub attached: Vec<Attached>,
    pub marks: Vec<Mark>,
    pub stats: MergeStats,
}

/// Lowercase letters and digits only, so "Save…" and "save" compare equal.
fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Whether two texts say the same thing: equal once normalised, or one inside
/// the other when it is long enough to mean something (a line of OCR against
/// a paragraph element).
pub fn same_text(a: &str, b: &str) -> bool {
    let (na, nb) = (norm(a), norm(b));
    if na.is_empty() || nb.is_empty() {
        return false;
    }
    na == nb
        || (na.chars().count() >= 3 && nb.contains(&na))
        || (nb.chars().count() >= 3 && na.contains(&nb))
}

/// Indices into `boxes` in reading order: lines top to bottom, each line left
/// to right. Boxes are on one line when their vertical centres are within half
/// the smaller height of the line's first box. Ties keep input order, so the
/// numbering is stable for the same input.
pub fn reading_order(boxes: &[Rect]) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..boxes.len()).collect();
    idx.sort_by(|&a, &b| {
        boxes[a]
            .center()
            .1
            .total_cmp(&boxes[b].center().1)
            .then(a.cmp(&b))
    });
    let mut out = Vec::with_capacity(idx.len());
    let mut line: Vec<usize> = Vec::new();
    let mut anchor: Option<(f64, f64)> = None;
    let flush = |line: &mut Vec<usize>, out: &mut Vec<usize>| {
        line.sort_by(|&a, &b| boxes[a].x.total_cmp(&boxes[b].x).then(a.cmp(&b)));
        out.append(line);
    };
    for i in idx {
        let (cy, h) = (boxes[i].center().1, boxes[i].h.max(1.0));
        match anchor {
            Some((acy, ah)) if (cy - acy).abs() <= 0.5 * h.min(ah) => line.push(i),
            _ => {
                flush(&mut line, &mut out);
                anchor = Some((cy, h));
                line.push(i);
            }
        }
    }
    flush(&mut line, &mut out);
    out
}

/// Join the boxes of every source to the accessibility elements.
///
/// Order of decisions for each box, in reading order: dropped when empty,
/// dropped when it lies in an occluded area, folded into an earlier box when
/// it is the same thing, dropped when it sits on a secure field, attached to
/// the smallest element it sits on (a control: attached; text the tree has:
/// dropped as a duplicate when it says the same, else passed over), and
/// otherwise numbered as a mark.
pub fn merge(ax: &[AxItem], sources: &[BoxSource], opts: &MergeOpts) -> Merged {
    let mut stats = MergeStats::default();

    // Flatten the sources, dropping what cannot be used.
    struct Entry<'a> {
        c: &'a Candidate,
        source: &'a str,
    }
    let mut entries: Vec<Entry> = Vec::new();
    for s in sources {
        for c in &s.boxes {
            stats.boxes_in += 1;
            if c.text.trim().is_empty()
                || !(c.rect.w > 0.0 && c.rect.h > 0.0)
                || !c.rect.x.is_finite()
                || !c.rect.y.is_finite()
            {
                stats.empty_dropped += 1;
                continue;
            }
            if opts
                .occluders
                .iter()
                .any(|o| o.contains_point(c.rect.center()) || c.rect.fraction_inside(o) >= 0.5)
            {
                stats.covered_dropped += 1;
                continue;
            }
            entries.push(Entry { c, source: &s.name });
        }
    }

    // Reading order first, so the first of two duplicates is the one that
    // keeps its place.
    let order = reading_order(&entries.iter().map(|e| e.c.rect).collect::<Vec<_>>());
    let mut kept: Vec<usize> = Vec::new();
    for i in order {
        let e = &entries[i];
        let twin = kept.iter().position(|&k| {
            let o = &entries[k];
            e.c.rect.iou(&o.c.rect) >= opts.duplicate_iou
                || (same_text(&e.c.text, &o.c.text)
                    && (e.c.rect.fraction_inside(&o.c.rect) >= 0.8
                        || o.c.rect.fraction_inside(&e.c.rect) >= 0.8))
        });
        match twin {
            Some(p) => {
                stats.duplicates += 1;
                // Keep the surer reading, in the first one's place.
                if e.c.confidence > entries[kept[p]].c.confidence {
                    kept[p] = i;
                }
            }
            None => kept.push(i),
        }
    }

    // Decide each kept box.
    enum Decision {
        Attach(usize),
        Mark,
    }
    let mut decided: Vec<(usize, Decision)> = Vec::new();
    for &i in &kept {
        let c = entries[i].c;
        let on = |it: &AxItem| {
            c.rect.fraction_inside(&it.rect) >= opts.containment || c.rect.iou(&it.rect) >= opts.iou
        };
        if ax.iter().any(|it| it.kind == AxKind::Secure && on(it)) {
            stats.secure_withheld += 1;
            continue;
        }
        let mut hits: Vec<usize> = (0..ax.len()).filter(|&j| on(&ax[j])).collect();
        hits.sort_by(|&a, &b| {
            ax[a]
                .rect
                .area()
                .total_cmp(&ax[b].rect.area())
                .then(a.cmp(&b))
        });
        let mut decision = Decision::Mark;
        let mut dropped = false;
        for j in hits {
            match ax[j].kind {
                AxKind::Control if ax[j].id.is_some() => {
                    decision = Decision::Attach(j);
                    break;
                }
                AxKind::Control | AxKind::Text => {
                    if ax[j].text.iter().any(|t| same_text(t, &c.text)) {
                        stats.same_as_ax += 1;
                        dropped = true;
                        break;
                    }
                }
                AxKind::Secure => {}
            }
        }
        if !dropped {
            decided.push((i, decision));
        }
    }

    // Number in reading order, then cap. Numbering happens before the cap so a
    // mark keeps its number whatever is left out after it.
    let mut out = Merged::default();
    let mut numbered = 0;
    let mut listed = 0;
    for (i, d) in decided {
        let e = &entries[i];
        if matches!(d, Decision::Mark) {
            numbered += 1;
        }
        if listed >= opts.max_listed {
            stats.omitted += 1;
            continue;
        }
        listed += 1;
        match d {
            Decision::Attach(j) => out.attached.push(Attached {
                id: ax[j].id.clone().unwrap_or_default(),
                text: e.c.text.clone(),
                rect: e.c.rect,
                confidence: e.c.confidence,
                source: e.source.to_string(),
                same_as_ax: ax[j].text.iter().any(|t| same_text(t, &e.c.text)),
            }),
            Decision::Mark => out.marks.push(Mark {
                id: format!("m{numbered}"),
                text: e.c.text.clone(),
                rect: e.c.rect,
                confidence: e.c.confidence,
                source: e.source.to_string(),
                unreliable: opts.unreliable,
            }),
        }
    }
    out.stats = stats;
    out
}

/// A mark to draw: its label and its rectangle in image pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct MarkDraw {
    pub label: String,
    pub rect: Rect,
}

/// Where a screen rectangle lands in a delivered image, given the screen point
/// of the image's top-left corner, the screen area it covers and its size in
/// pixels (the mapping `capture_window` reports).
pub fn screen_to_image(
    r: &Rect,
    origin: (f64, f64),
    screen_size: (f64, f64),
    image: (u32, u32),
) -> Rect {
    let sx = if screen_size.0 > 0.0 {
        image.0 as f64 / screen_size.0
    } else {
        1.0
    };
    let sy = if screen_size.1 > 0.0 {
        image.1 as f64 / screen_size.1
    } else {
        1.0
    };
    Rect::new(
        (r.x - origin.0) * sx,
        (r.y - origin.1) * sy,
        r.w * sx,
        r.h * sy,
    )
}

/// Outline and label colours, cycled so neighbouring marks differ.
const PALETTE: [[u8; 3]; 4] = [[230, 60, 0], [0, 110, 230], [0, 150, 70], [170, 0, 170]];

/// Draw each mark's outline and number onto PNG bytes and return the new PNG.
/// Marks that fall wholly outside the image are skipped; the rest are clipped.
pub fn draw_marks(png_bytes: &[u8], marks: &[MarkDraw]) -> Result<Vec<u8>, String> {
    let (w, h, mut px) = decode_rgba(png_bytes)?;
    // Same text scale rule as the grid: bigger images get bigger numbers.
    let ts = ((w.max(h) as f64 / 700.0).round() as i64).clamp(1, 3);
    let mut c = Canvas { px: &mut px, w, h };
    for (i, m) in marks.iter().enumerate() {
        let rgb = PALETTE[i % PALETTE.len()];
        let (x0, y0) = (m.rect.x.floor() as i64, m.rect.y.floor() as i64);
        let (x1, y1) = (
            (m.rect.x + m.rect.w).ceil() as i64,
            (m.rect.y + m.rect.h).ceil() as i64,
        );
        if x1 <= 0 || y1 <= 0 || x0 >= w as i64 || y0 >= h as i64 {
            continue;
        }
        for (rx, ry, rw, rh) in [
            (x0, y0, x1 - x0, ts),
            (x0, y1 - ts, x1 - x0, ts),
            (x0, y0, ts, y1 - y0),
            (x1 - ts, y0, ts, y1 - y0),
        ] {
            c.fill(
                &PixRect {
                    x: rx,
                    y: ry,
                    w: rw,
                    h: rh,
                },
                rgb,
                1.0,
            );
        }
        // The number sits just above the box, or inside its top-left corner
        // when there is no room above.
        let (_, lh) = label_size(&m.label, ts);
        let ly = if y0 - lh >= 0 { y0 - lh } else { y0 };
        c.label_on(x0, ly, &m.label, ts, rgb, 1.0);
    }
    encode_rgba(&px, w, h)
}

/// As [`draw_marks`], for a base64 PNG as captures carry it.
pub fn draw_marks_b64(png_b64: &str, marks: &[MarkDraw]) -> Result<String, String> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(png_b64.trim())
        .map_err(|e| format!("image is not valid base64: {e}"))?;
    let out = draw_marks(&bytes, marks)?;
    Ok(base64::engine::general_purpose::STANDARD.encode(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect::new(x, y, w, h)
    }

    fn cand(text: &str, rect: Rect) -> Candidate {
        Candidate {
            text: text.into(),
            rect,
            confidence: 0.9,
        }
    }

    fn ocr(boxes: Vec<Candidate>) -> Vec<BoxSource> {
        vec![BoxSource {
            name: "ocr".into(),
            boxes,
        }]
    }

    fn control(id: &str, rect: Rect, text: &[&str]) -> AxItem {
        AxItem {
            id: Some(id.into()),
            kind: AxKind::Control,
            rect,
            text: text.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn static_text(rect: Rect, text: &str) -> AxItem {
        AxItem {
            id: None,
            kind: AxKind::Text,
            rect,
            text: vec![text.into()],
        }
    }

    #[test]
    fn rect_arithmetic_is_what_the_matching_relies_on() {
        let a = r(0.0, 0.0, 10.0, 10.0);
        let b = r(5.0, 0.0, 10.0, 10.0);
        assert_eq!(a.intersection(&b), 50.0);
        assert!((a.iou(&b) - 50.0 / 150.0).abs() < 1e-9);
        assert_eq!(r(2.0, 2.0, 4.0, 4.0).fraction_inside(&a), 1.0);
        assert_eq!(b.fraction_inside(&a), 0.5);
        assert_eq!(a.intersection(&r(20.0, 20.0, 5.0, 5.0)), 0.0);
        assert_eq!(r(0.0, 0.0, 0.0, 5.0).iou(&r(0.0, 0.0, 0.0, 5.0)), 0.0);
        assert_eq!(r(0.0, 0.0, 0.0, 5.0).fraction_inside(&a), 0.0);
        assert!(a.contains_point((10.0, 10.0)) && !a.contains_point((10.1, 5.0)));
    }

    #[test]
    fn reading_order_is_lines_top_down_then_left_to_right() {
        // Three on one line (the middle one a little lower), one below.
        let boxes = [
            r(200.0, 12.0, 50.0, 20.0), // 0: right of the first line
            r(10.0, 10.0, 50.0, 20.0),  // 1: left of the first line
            r(100.0, 14.0, 50.0, 20.0), // 2: middle of the first line
            r(10.0, 80.0, 50.0, 20.0),  // 3: second line
        ];
        assert_eq!(reading_order(&boxes), vec![1, 2, 0, 3]);
        assert!(reading_order(&[]).is_empty());
    }

    #[test]
    fn reading_order_keeps_input_order_for_identical_positions() {
        let b = r(0.0, 0.0, 10.0, 10.0);
        assert_eq!(reading_order(&[b, b, b]), vec![0, 1, 2]);
    }

    #[test]
    fn a_box_with_no_element_becomes_a_mark_with_its_rectangle() {
        let m = merge(
            &[],
            &ocr(vec![cand("Play", r(40.0, 300.0, 60.0, 20.0))]),
            &MergeOpts::default(),
        );
        assert!(m.attached.is_empty());
        assert_eq!(m.marks.len(), 1);
        assert_eq!(m.marks[0].id, "m1");
        assert_eq!(m.marks[0].text, "Play");
        assert_eq!(m.marks[0].rect, r(40.0, 300.0, 60.0, 20.0));
        assert!(!m.marks[0].unreliable);
        assert_eq!(m.stats.boxes_in, 1);
    }

    #[test]
    fn marks_are_numbered_in_reading_order_not_input_order() {
        let m = merge(
            &[],
            &ocr(vec![
                cand("third", r(10.0, 200.0, 40.0, 16.0)),
                cand("second", r(300.0, 20.0, 40.0, 16.0)),
                cand("first", r(10.0, 20.0, 40.0, 16.0)),
            ]),
            &MergeOpts::default(),
        );
        let order: Vec<(&str, &str)> = m
            .marks
            .iter()
            .map(|k| (k.id.as_str(), k.text.as_str()))
            .collect();
        assert_eq!(
            order,
            vec![("m1", "first"), ("m2", "second"), ("m3", "third")]
        );
    }

    #[test]
    fn numbering_is_stable_for_the_same_input() {
        let boxes = ocr(vec![
            cand("a", r(10.0, 10.0, 30.0, 14.0)),
            cand("b", r(60.0, 10.0, 30.0, 14.0)),
            cand("c", r(10.0, 60.0, 30.0, 14.0)),
        ]);
        let one = merge(&[], &boxes, &MergeOpts::default());
        let two = merge(&[], &boxes, &MergeOpts::default());
        assert_eq!(one, two);
    }

    #[test]
    fn a_box_inside_a_control_is_attached_to_its_ref() {
        let ax = [control("@e7", r(0.0, 0.0, 400.0, 300.0), &["Notes"])];
        let m = merge(
            &ax,
            &ocr(vec![cand("hello world", r(20.0, 40.0, 90.0, 16.0))]),
            &MergeOpts::default(),
        );
        assert!(m.marks.is_empty());
        assert_eq!(m.attached.len(), 1);
        assert_eq!(m.attached[0].id, "@e7");
        assert_eq!(m.attached[0].text, "hello world");
        assert!(!m.attached[0].same_as_ax);
    }

    #[test]
    fn text_that_repeats_the_controls_own_text_is_attached_and_says_so() {
        let ax = [control("@e1", r(0.0, 0.0, 100.0, 30.0), &["Save"])];
        let m = merge(
            &ax,
            &ocr(vec![cand("Save", r(30.0, 6.0, 40.0, 16.0))]),
            &MergeOpts::default(),
        );
        assert_eq!(m.attached.len(), 1);
        assert!(m.attached[0].same_as_ax);
    }

    #[test]
    fn the_smallest_element_under_a_box_wins() {
        let ax = [
            control("@e1", r(0.0, 0.0, 800.0, 600.0), &[]),
            control("@e2", r(100.0, 100.0, 200.0, 40.0), &[]),
        ];
        let m = merge(
            &ax,
            &ocr(vec![cand("Search", r(110.0, 108.0, 60.0, 16.0))]),
            &MergeOpts::default(),
        );
        assert_eq!(m.attached[0].id, "@e2");
    }

    #[test]
    fn text_the_tree_already_has_is_a_duplicate_not_a_mark() {
        let ax = [static_text(r(10.0, 10.0, 120.0, 18.0), "Welcome back")];
        let m = merge(
            &ax,
            &ocr(vec![cand("Welcome back", r(12.0, 11.0, 116.0, 16.0))]),
            &MergeOpts::default(),
        );
        assert!(m.marks.is_empty() && m.attached.is_empty());
        assert_eq!(m.stats.same_as_ax, 1);
    }

    #[test]
    fn text_over_static_text_that_says_something_else_is_still_a_mark() {
        // The tree names a label; the pixels show another word on top of it
        // (text drawn into an image). The tree does not cover it.
        let ax = [static_text(r(10.0, 10.0, 120.0, 18.0), "Welcome back")];
        let m = merge(
            &ax,
            &ocr(vec![cand("Sale ends today", r(12.0, 11.0, 116.0, 16.0))]),
            &MergeOpts::default(),
        );
        assert_eq!(m.marks.len(), 1);
        assert_eq!(m.stats.same_as_ax, 0);
    }

    #[test]
    fn a_control_beneath_a_label_of_other_text_still_takes_the_attachment() {
        // A static-text node of a different text sits inside a button; the box
        // belongs to the button, which is what can be acted on.
        let ax = [
            control("@e3", r(0.0, 0.0, 200.0, 40.0), &[]),
            static_text(r(10.0, 10.0, 60.0, 18.0), "Other"),
        ];
        let m = merge(
            &ax,
            &ocr(vec![cand("Download", r(12.0, 11.0, 56.0, 16.0))]),
            &MergeOpts::default(),
        );
        assert_eq!(m.attached.len(), 1);
        assert_eq!(m.attached[0].id, "@e3");
    }

    #[test]
    fn a_box_on_a_secure_field_is_withheld_and_counted() {
        let ax = [
            control("@e1", r(0.0, 0.0, 500.0, 400.0), &[]),
            AxItem {
                id: Some("@e2".into()),
                kind: AxKind::Secure,
                rect: r(100.0, 100.0, 200.0, 24.0),
                text: vec![],
            },
        ];
        let m = merge(
            &ax,
            &ocr(vec![
                cand(
                    "\u{2022}\u{2022}\u{2022}\u{2022}",
                    r(104.0, 104.0, 60.0, 16.0),
                ),
                cand("Password", r(20.0, 104.0, 70.0, 16.0)),
            ]),
            &MergeOpts::default(),
        );
        assert_eq!(m.stats.secure_withheld, 1);
        assert!(m.marks.iter().all(|k| !k.text.contains('\u{2022}')));
        assert!(m.attached.iter().all(|k| !k.text.contains('\u{2022}')));
        // The label beside it is ordinary text of the window.
        assert_eq!(m.attached.len(), 1);
        assert_eq!(m.attached[0].text, "Password");
    }

    #[test]
    fn boxes_from_two_sources_for_one_thing_are_one_entry_with_the_surer_reading() {
        let sources = vec![
            BoxSource {
                name: "ocr".into(),
                boxes: vec![Candidate {
                    text: "Settings".into(),
                    rect: r(10.0, 10.0, 80.0, 20.0),
                    confidence: 0.6,
                }],
            },
            BoxSource {
                name: "grounding".into(),
                boxes: vec![Candidate {
                    text: "Settings".into(),
                    rect: r(11.0, 10.0, 80.0, 20.0),
                    confidence: 0.95,
                }],
            },
        ];
        let m = merge(&[], &sources, &MergeOpts::default());
        assert_eq!(m.marks.len(), 1);
        assert_eq!(m.marks[0].source, "grounding");
        assert_eq!(m.stats.duplicates, 1);
        assert_eq!(m.stats.boxes_in, 2);
    }

    #[test]
    fn a_source_with_a_box_the_other_lacks_adds_a_mark() {
        let sources = vec![
            BoxSource {
                name: "ocr".into(),
                boxes: vec![cand("Open", r(10.0, 10.0, 40.0, 16.0))],
            },
            BoxSource {
                name: "grounding".into(),
                boxes: vec![cand("gear icon", r(300.0, 10.0, 24.0, 24.0))],
            },
        ];
        let m = merge(&[], &sources, &MergeOpts::default());
        let got: Vec<(&str, &str)> = m
            .marks
            .iter()
            .map(|k| (k.text.as_str(), k.source.as_str()))
            .collect();
        assert_eq!(got, vec![("Open", "ocr"), ("gear icon", "grounding")]);
    }

    #[test]
    fn boxes_in_an_area_another_app_covers_are_dropped_and_counted() {
        let opts = MergeOpts {
            occluders: vec![r(200.0, 0.0, 200.0, 400.0)],
            ..MergeOpts::default()
        };
        let m = merge(
            &[],
            &ocr(vec![
                cand("mine", r(20.0, 20.0, 60.0, 16.0)),
                cand("theirs", r(250.0, 20.0, 60.0, 16.0)),
                // Mostly outside the covered area, so it stays.
                cand("beside", r(140.0, 60.0, 100.0, 16.0)),
            ]),
            &opts,
        );
        let texts: Vec<&str> = m.marks.iter().map(|k| k.text.as_str()).collect();
        assert_eq!(texts, vec!["mine", "beside"]);
        assert_eq!(m.stats.covered_dropped, 1);
    }

    #[test]
    fn marks_are_flagged_unreliable_when_the_cover_is_known_but_not_where() {
        let opts = MergeOpts {
            unreliable: true,
            ..MergeOpts::default()
        };
        let m = merge(&[], &ocr(vec![cand("x1", r(0.0, 0.0, 20.0, 10.0))]), &opts);
        assert!(m.marks[0].unreliable);
    }

    #[test]
    fn empty_and_degenerate_boxes_are_dropped_and_counted() {
        let m = merge(
            &[],
            &ocr(vec![
                cand("   ", r(0.0, 0.0, 20.0, 10.0)),
                cand("flat", r(0.0, 0.0, 20.0, 0.0)),
                cand("nan", r(f64::NAN, 0.0, 20.0, 10.0)),
                cand("ok", r(0.0, 40.0, 20.0, 10.0)),
            ]),
            &MergeOpts::default(),
        );
        assert_eq!(m.marks.len(), 1);
        assert_eq!(m.stats.empty_dropped, 3);
        assert_eq!(m.stats.boxes_in, 4);
    }

    #[test]
    fn the_cap_counts_what_it_leaves_out_and_keeps_the_numbers() {
        let boxes: Vec<Candidate> = (0..10)
            .map(|i| {
                cand(
                    &format!("w{i}"),
                    r(10.0, 10.0 + 40.0 * i as f64, 50.0, 16.0),
                )
            })
            .collect();
        let opts = MergeOpts {
            max_listed: 4,
            ..MergeOpts::default()
        };
        let m = merge(&[], &ocr(boxes), &opts);
        assert_eq!(m.marks.len(), 4);
        assert_eq!(m.stats.omitted, 6);
        let ids: Vec<&str> = m.marks.iter().map(|k| k.id.as_str()).collect();
        assert_eq!(ids, vec!["m1", "m2", "m3", "m4"]);
    }

    #[test]
    fn attachments_and_marks_share_one_cap() {
        let ax = [control("@e1", r(0.0, 0.0, 300.0, 100.0), &[])];
        let m = merge(
            &ax,
            &ocr(vec![
                cand("in-1", r(10.0, 10.0, 50.0, 16.0)),
                cand("in-2", r(10.0, 40.0, 50.0, 16.0)),
                cand("out-1", r(10.0, 200.0, 50.0, 16.0)),
            ]),
            &MergeOpts {
                max_listed: 2,
                ..MergeOpts::default()
            },
        );
        assert_eq!(m.attached.len() + m.marks.len(), 2);
        assert_eq!(m.stats.omitted, 1);
        assert!(m.marks.is_empty());
    }

    #[test]
    fn same_text_ignores_case_and_punctuation_and_short_fragments() {
        assert!(same_text("Save\u{2026}", "save"));
        assert!(same_text("hello", "Hello world, again"));
        assert!(!same_text("a", "abc"));
        assert!(!same_text("", ""));
        assert!(!same_text("Open", "Close"));
    }

    #[test]
    fn screen_rectangles_map_into_a_scaled_image() {
        // A 400x300 point window at (50, 20) delivered at 800x600.
        let img = screen_to_image(
            &r(150.0, 70.0, 40.0, 10.0),
            (50.0, 20.0),
            (400.0, 300.0),
            (800, 600),
        );
        assert_eq!(img, r(200.0, 100.0, 80.0, 20.0));
        // A zero-sized area does not divide by zero.
        let flat = screen_to_image(&r(1.0, 1.0, 2.0, 2.0), (0.0, 0.0), (0.0, 0.0), (10, 10));
        assert_eq!(flat, r(1.0, 1.0, 2.0, 2.0));
    }

    fn white_png(w: u32, h: u32) -> Vec<u8> {
        encode_rgba(&vec![255u8; (w * h * 4) as usize], w, h).unwrap()
    }

    fn px(png: &[u8], x: u32, y: u32) -> [u8; 3] {
        let (w, _, rgba) = decode_rgba(png).unwrap();
        let i = ((y * w + x) * 4) as usize;
        [rgba[i], rgba[i + 1], rgba[i + 2]]
    }

    #[test]
    fn drawing_outlines_a_mark_and_labels_it_leaving_the_inside_alone() {
        let png = white_png(300, 200);
        let out = draw_marks(
            &png,
            &[MarkDraw {
                label: "m1".into(),
                rect: r(100.0, 80.0, 120.0, 50.0),
            }],
        )
        .unwrap();
        // The outline is the first palette colour on all four edges.
        for (x, y) in [(150, 80), (150, 129), (100, 100), (219, 100)] {
            assert_eq!(px(&out, x, y), PALETTE[0], "edge pixel {x},{y}");
        }
        // The inside of the box is the screenshot's own pixels.
        assert_eq!(px(&out, 160, 105), [255, 255, 255]);
        // The label box sits above the top-left corner, in the same colour.
        assert_eq!(px(&out, 100, 80 - 3), PALETTE[0]);
        // Far from any mark nothing changed.
        assert_eq!(px(&out, 10, 10), [255, 255, 255]);
        // Still a PNG of the same size.
        let (w, h, _) = decode_rgba(&out).unwrap();
        assert_eq!((w, h), (300, 200));
    }

    #[test]
    fn drawing_clips_a_mark_at_the_edge_and_skips_one_outside_the_image() {
        let png = white_png(100, 100);
        let out = draw_marks(
            &png,
            &[
                MarkDraw {
                    label: "m1".into(),
                    rect: r(80.0, 80.0, 60.0, 60.0),
                },
                MarkDraw {
                    label: "m2".into(),
                    rect: r(500.0, 500.0, 10.0, 10.0),
                },
            ],
        )
        .unwrap();
        assert_eq!(px(&out, 80, 90), PALETTE[0]);
        assert_eq!(px(&out, 99, 99), [255, 255, 255]);
    }

    #[test]
    fn a_mark_at_the_top_edge_is_labelled_inside_its_box() {
        let png = white_png(200, 100);
        let out = draw_marks(
            &png,
            &[MarkDraw {
                label: "m1".into(),
                rect: r(20.0, 0.0, 100.0, 40.0),
            }],
        )
        .unwrap();
        assert_eq!(px(&out, 32, 6), PALETTE[0]);
    }

    #[test]
    fn drawing_rejects_what_is_not_a_png() {
        assert!(draw_marks(b"nope", &[]).is_err());
    }
}
