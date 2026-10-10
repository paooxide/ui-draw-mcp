//! A labelled coordinate grid drawn onto a PNG.
//!
//! Models are poor at reading a pixel position off a bare screenshot, and worse
//! when the image is scaled (a Retina capture, a device-pixel-ratio-2 page, a
//! downscale for token cost). The grid puts the *click* coordinate system on
//! the picture itself: every line is labelled with the number a click tool
//! takes (CSS px for `browser_act`, screen points for `mouse_action`), however
//! the image was scaled, so the model reads a number instead of estimating one.
//!
//! Shared by the desktop captures here and by `browser_screenshot`, which is
//! why it works on encoded PNG bytes and knows nothing about either caller.

use std::io::Cursor;

use base64::Engine;
use serde_json::Value;

/// Spacing used when the caller asks for a grid and gives none.
pub const DEFAULT_STEP: u32 = 100;
/// Tighter than this and the labels cover the picture they annotate.
pub const MIN_STEP: u32 = 25;

/// Where the grid sits and how the image relates to the click coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridSpec {
    /// Distance between lines, in click units.
    pub step: u32,
    /// Image pixels per click unit, per axis (2.0 for a 2x page; 0.5 for a
    /// capture delivered at half the size of the area it covers).
    pub px_per_unit: (f64, f64),
    /// The click coordinate of the image's top-left pixel: `(0, 0)` for a
    /// viewport, the window's position for a window capture, the element's
    /// position for an element.
    pub origin: (f64, f64),
}

/// Read `grid` / `grid_step` from tool arguments: `None` when no grid is
/// wanted, otherwise the spacing, clamped to [`MIN_STEP`].
pub fn step_from_args(args: &Value) -> Option<u32> {
    if !args.get("grid").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    let step = args
        .get("grid_step")
        .and_then(Value::as_f64)
        .filter(|s| s.is_finite())
        .map(|s| s.round().clamp(MIN_STEP as f64, 100_000.0) as u32)
        .unwrap_or(DEFAULT_STEP);
    Some(step)
}

/// The lines that fall inside an axis of `extent_px` pixels, as
/// `(label, pixel)`. A line is at every multiple of `step` in click units, so
/// a window that starts at x=37 gets its first line at 100, 63 units in.
pub fn grid_lines(origin: f64, step: u32, px_per_unit: f64, extent_px: u32) -> Vec<(i64, u32)> {
    if step == 0 || px_per_unit <= 0.0 || !px_per_unit.is_finite() || !origin.is_finite() {
        return Vec::new();
    }
    let step = step as i64;
    let mut k = (origin / step as f64).ceil() as i64;
    let mut out = Vec::new();
    loop {
        let label = k * step;
        let px = ((label as f64 - origin) * px_per_unit).round();
        if px >= extent_px as f64 {
            break;
        }
        if px >= 0.0 {
            out.push((label, px as u32));
        }
        k += 1;
        // A grid never has this many lines; stop a degenerate spec early.
        if out.len() > 4096 {
            break;
        }
    }
    out
}

/// Draw the grid on a base64 PNG and return the new base64 PNG.
pub fn draw_grid_b64(png_b64: &str, spec: &GridSpec) -> Result<String, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(png_b64.trim())
        .map_err(|e| format!("image is not valid base64: {e}"))?;
    let out = draw_grid(&bytes, spec)?;
    Ok(base64::engine::general_purpose::STANDARD.encode(out))
}

/// Decode a base64 PNG to 8-bit RGBA pixels: `(width, height, pixels)`.
///
/// Public because `browser_screenshot` feeds the capture it already has to
/// the recogniser, and should not need a second PNG decoder to do it.
pub fn decode_rgba_b64(png_b64: &str) -> Result<(u32, u32, Vec<u8>), String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(png_b64.trim())
        .map_err(|e| format!("image is not valid base64: {e}"))?;
    decode_rgba(&bytes)
}

/// Draw the grid on PNG bytes and return the new PNG bytes (RGBA).
pub fn draw_grid(png_bytes: &[u8], spec: &GridSpec) -> Result<Vec<u8>, String> {
    let (w, h, mut px) = decode_rgba(png_bytes)?;
    draw_on(&mut px, w, h, spec);
    encode_rgba(&px, w, h)
}

fn decode_rgba(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let mut dec = png::Decoder::new(Cursor::new(bytes));
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec.read_info().map_err(|e| format!("not a PNG: {e}"))?;
    let size = reader
        .output_buffer_size()
        .ok_or("image is too large to draw on")?;
    let mut buf = vec![0u8; size];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| format!("cannot decode PNG: {e}"))?;
    let (w, h) = (info.width, info.height);
    let data = &buf[..info.buffer_size()];
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => data.to_vec(),
        png::ColorType::Rgb => data
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::Grayscale => data.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::GrayscaleAlpha => data
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Indexed => return Err("indexed PNG was not expanded".into()),
    };
    Ok((w, h, rgba))
}

fn encode_rgba(px: &[u8], w: u32, h: u32) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut wr = enc
            .write_header()
            .map_err(|e| format!("cannot encode PNG: {e}"))?;
        wr.write_image_data(px)
            .map_err(|e| format!("cannot encode PNG: {e}"))?;
    }
    Ok(out)
}

/// Line colour: saturated magenta reads on white, black and mid-grey pages.
const LINE: [u8; 3] = [255, 0, 200];
const LINE_ALPHA: f32 = 0.7;
/// Labels sit on a dark box with white text, so they read on any background.
const BOX_ALPHA: f32 = 0.8;
/// Below this many image pixels between lines there is no room to label every
/// crossing, so labels go on the top and left edges only.
const CROSSING_LABEL_MIN_CELL: f64 = 64.0;

/// A rectangle in image pixels.
struct Rect {
    x: i64,
    y: i64,
    w: i64,
    h: i64,
}

/// The pixel buffer being drawn on.
struct Canvas<'a> {
    px: &'a mut [u8],
    w: u32,
    h: u32,
}

impl Canvas<'_> {
    fn blend(&mut self, x: i64, y: i64, rgb: [u8; 3], alpha: f32) {
        if x < 0 || y < 0 || x >= self.w as i64 || y >= self.h as i64 {
            return;
        }
        let i = ((y as u32 * self.w + x as u32) * 4) as usize;
        for (c, v) in rgb.iter().enumerate() {
            self.px[i + c] =
                (self.px[i + c] as f32 * (1.0 - alpha) + *v as f32 * alpha).round() as u8;
        }
        self.px[i + 3] = 255;
    }

    fn fill(&mut self, r: &Rect, rgb: [u8; 3], alpha: f32) {
        for y in r.y..r.y + r.h {
            for x in r.x..r.x + r.w {
                self.blend(x, y, rgb, alpha);
            }
        }
    }

    /// Draw `text` in a filled box whose top-left corner is `(x, y)`, pulled
    /// back inside the image when it would overhang the right or bottom edge.
    fn label(&mut self, x: i64, y: i64, text: &str, ts: i64) {
        let (bw, bh) = label_size(text, ts);
        let x = x.min(self.w as i64 - bw).max(0);
        let y = y.min(self.h as i64 - bh).max(0);
        self.fill(&Rect { x, y, w: bw, h: bh }, [0, 0, 0], BOX_ALPHA);
        for (i, c) in text.chars().enumerate() {
            let gx = x + (1 + i as i64 * 6) * ts;
            let gy = y + ts;
            for (row, bits) in glyph(c).iter().enumerate() {
                for col in 0..5i64 {
                    if bits & (0x10 >> col) != 0 {
                        let cell = Rect {
                            x: gx + col * ts,
                            y: gy + row as i64 * ts,
                            w: ts,
                            h: ts,
                        };
                        self.fill(&cell, [255, 255, 255], 1.0);
                    }
                }
            }
        }
    }
}

/// 5x7 glyphs for the characters a coordinate label uses; one row per byte,
/// bit 4 is the leftmost column.
fn glyph(c: char) -> [u8; 7] {
    match c {
        '0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        '1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        '2' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        '3' => [0x1E, 0x01, 0x01, 0x0E, 0x01, 0x01, 0x1E],
        '4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        '5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        '6' => [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        '7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        '9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
        ',' => [0, 0, 0, 0, 0x04, 0x04, 0x08],
        '-' => [0, 0, 0, 0x1F, 0, 0, 0],
        _ => [0; 7],
    }
}

/// Size of a label box in pixels for `text` at text scale `ts`.
fn label_size(text: &str, ts: i64) -> (i64, i64) {
    let n = text.chars().count() as i64;
    ((n * 6 - 1 + 2) * ts, (7 + 2) * ts)
}

fn draw_on(px: &mut [u8], w: u32, h: u32, spec: &GridSpec) {
    let xs = grid_lines(spec.origin.0, spec.step, spec.px_per_unit.0, w);
    let ys = grid_lines(spec.origin.1, spec.step, spec.px_per_unit.1, h);
    // 5x7 text at 1x for a ~700px edge; bigger images get bigger text so a
    // picture sized for the model stays legible.
    let ts = ((w.max(h) as f64 / 700.0).round() as i64).clamp(1, 3);
    let mut c = Canvas { px, w, h };

    for &(_, x) in &xs {
        c.fill(
            &Rect {
                x: x as i64,
                y: 0,
                w: 1,
                h: h as i64,
            },
            LINE,
            LINE_ALPHA,
        );
    }
    for &(_, y) in &ys {
        c.fill(
            &Rect {
                x: 0,
                y: y as i64,
                w: w as i64,
                h: 1,
            },
            LINE,
            LINE_ALPHA,
        );
    }

    let cell = (spec.step as f64 * spec.px_per_unit.0).min(spec.step as f64 * spec.px_per_unit.1);
    if cell >= CROSSING_LABEL_MIN_CELL {
        for &(lx, x) in &xs {
            for &(ly, y) in &ys {
                c.label(x as i64 + 2, y as i64 + 2, &format!("{lx},{ly}"), ts);
            }
        }
    } else {
        let (_, bh) = label_size("0", ts);
        for &(lx, x) in &xs {
            c.label(x as i64 + 2, 0, &lx.to_string(), ts);
        }
        for &(ly, y) in &ys {
            // The top strip belongs to the x labels.
            if y as i64 + 2 >= bh {
                c.label(0, y as i64 + 2, &ly.to_string(), ts);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn white(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut wr = enc.write_header().unwrap();
        wr.write_image_data(&vec![255u8; (w * h * 3) as usize])
            .unwrap();
        drop(wr);
        out
    }

    fn at(rgba: &[u8], w: u32, x: u32, y: u32) -> [u8; 3] {
        let i = ((y * w + x) * 4) as usize;
        [rgba[i], rgba[i + 1], rgba[i + 2]]
    }

    fn spec(step: u32, ppu: f64, origin: (f64, f64)) -> GridSpec {
        GridSpec {
            step,
            px_per_unit: (ppu, ppu),
            origin,
        }
    }

    fn draw(w: u32, h: u32, spec: GridSpec) -> Vec<u8> {
        let out = draw_grid(&white(w, h), &spec).unwrap();
        let (ow, oh, px) = decode_rgba(&out).unwrap();
        assert_eq!((ow, oh), (w, h), "the grid must not resize the image");
        px
    }

    const WHITE: [u8; 3] = [255, 255, 255];

    #[test]
    fn lines_follow_the_origin_and_scale() {
        assert_eq!(
            grid_lines(0.0, 100, 1.0, 350),
            vec![(0, 0), (100, 100), (200, 200), (300, 300)]
        );
        // A window that starts at x=37: the first line is 100, 63 units in.
        assert_eq!(
            grid_lines(37.0, 100, 1.0, 400),
            vec![(100, 63), (200, 163), (300, 263), (400, 363)]
        );
        // 2x: the same labels, twice the pixels apart.
        assert_eq!(
            grid_lines(0.0, 100, 2.0, 500),
            vec![(0, 0), (100, 200), (200, 400)]
        );
        // A negative origin (a display left of the main one) labels negatives.
        assert_eq!(
            grid_lines(-150.0, 100, 1.0, 200),
            vec![(-100, 50), (0, 150)]
        );
        // A capture delivered smaller than the area it covers.
        assert_eq!(
            grid_lines(0.0, 100, 0.5, 120),
            vec![(0, 0), (100, 50), (200, 100)]
        );
    }

    #[test]
    fn step_is_clamped_and_grid_is_opt_in() {
        use serde_json::json;
        assert_eq!(step_from_args(&json!({})), None);
        assert_eq!(step_from_args(&json!({"grid": false})), None);
        assert_eq!(step_from_args(&json!({"grid": true})), Some(100));
        assert_eq!(
            step_from_args(&json!({"grid": true, "grid_step": 10})),
            Some(25)
        );
        assert_eq!(
            step_from_args(&json!({"grid": true, "grid_step": 50})),
            Some(50)
        );
    }

    #[test]
    fn line_pixels_land_at_the_step_in_a_1x_image() {
        let px = draw(400, 400, spec(100, 1.0, (0.0, 0.0)));
        // On the x=100 line, clear of the labels at the crossings.
        assert_ne!(at(&px, 400, 100, 250), WHITE);
        assert_eq!(at(&px, 400, 99, 250), WHITE);
        assert_eq!(at(&px, 400, 101, 250), WHITE);
        assert_eq!(at(&px, 400, 150, 250), WHITE);
        // And on the y=200 line.
        assert_ne!(at(&px, 400, 150, 200), WHITE);
    }

    #[test]
    fn at_2x_lines_are_twice_the_pixels_apart_but_still_one_step_apart() {
        let px = draw(800, 800, spec(100, 2.0, (0.0, 0.0)));
        assert_ne!(at(&px, 800, 200, 500), WHITE, "x=100 is pixel 200");
        assert_eq!(at(&px, 800, 100, 500), WHITE, "pixel 100 is x=50");
        assert_ne!(at(&px, 800, 400, 500), WHITE, "x=200 is pixel 400");
    }

    #[test]
    fn an_origin_offset_moves_the_lines_not_the_labels() {
        let px = draw(400, 400, spec(100, 1.0, (37.0, 0.0)));
        // x=100 is 63 pixels into an image whose left edge is x=37.
        assert_ne!(at(&px, 400, 63, 250), WHITE);
        assert_eq!(at(&px, 400, 100, 250), WHITE);
    }

    #[test]
    fn labels_are_white_text_in_a_dark_box_that_reads_on_a_light_page() {
        // Step 25 at 1x is too tight for crossing labels: edge labels only.
        let px = draw(400, 400, spec(25, 1.0, (0.0, 0.0)));
        // The box of the "100" label starts two pixels right of the x=100 line,
        // in the top strip; its corner is dark where the page is white.
        let c = at(&px, 400, 102, 0);
        assert!(c.iter().all(|&v| v < 80), "label box is dark, got {c:?}");
        // Some pixel inside the box is the white of a glyph.
        let any_white = (102..120).any(|x| (0..9).any(|y| at(&px, 400, x, y) == WHITE));
        assert!(any_white, "glyph pixels are white");
    }

    #[test]
    fn crossings_are_labelled_with_both_coordinates() {
        let px = draw(400, 400, spec(100, 1.0, (0.0, 0.0)));
        // The box for "200,300" begins just right of and below the crossing.
        let c = at(&px, 400, 202, 302);
        assert!(c.iter().all(|&v| v < 80), "{c:?}");
    }

    #[test]
    fn non_png_input_is_an_error_not_a_panic() {
        let s = spec(100, 1.0, (0.0, 0.0));
        assert!(draw_grid(b"not a png", &s).is_err());
        assert!(draw_grid_b64("!!!", &s).is_err());
    }

    #[test]
    fn base64_round_trip_keeps_dimensions() {
        let b64 = base64::engine::general_purpose::STANDARD.encode(white(120, 80));
        let out = draw_grid_b64(&b64, &spec(25, 1.0, (0.0, 0.0))).unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(out)
            .unwrap();
        let (w, h, _) = decode_rgba(&bytes).unwrap();
        assert_eq!((w, h), (120, 80));
    }
}
