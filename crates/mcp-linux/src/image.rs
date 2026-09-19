//! PNG in, PNG out: decode, crop, downscale, encode, and the tiny thumbnail
//! that stands in for "did the screen change?".
//!
//! The macOS backend leans on `sips` for this. Linux has no such tool
//! everywhere, and pulling in a full image crate for four operations is more
//! surface than the operations deserve, so this is the `png` crate plus a
//! box filter.

use std::io::Cursor;

use base64::Engine;

/// Decoded 8-bit RGB pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rgb {
    pub width: u32,
    pub height: u32,
    /// `width * height * 3` bytes, row-major.
    pub data: Vec<u8>,
}

impl Rgb {
    pub fn new(width: u32, height: u32, data: Vec<u8>) -> Result<Rgb, String> {
        let expected = width as usize * height as usize * 3;
        if data.len() != expected {
            return Err(format!(
                "rgb buffer is {} bytes, expected {expected} for {width}x{height}",
                data.len()
            ));
        }
        Ok(Rgb {
            width,
            height,
            data,
        })
    }

    /// Crop to a rectangle, clamped to the image. An empty intersection is
    /// an error rather than a zero-sized image, because nothing downstream
    /// can use one.
    pub fn crop(&self, x: i64, y: i64, w: i64, h: i64) -> Result<Rgb, String> {
        let x0 = x.clamp(0, self.width as i64);
        let y0 = y.clamp(0, self.height as i64);
        let x1 = (x + w).clamp(0, self.width as i64);
        let y1 = (y + h).clamp(0, self.height as i64);
        if x1 <= x0 || y1 <= y0 {
            return Err(format!(
                "region ({x}, {y}, {w}, {h}) does not intersect the {}x{} image",
                self.width, self.height
            ));
        }
        let (cw, ch) = ((x1 - x0) as usize, (y1 - y0) as usize);
        let mut out = Vec::with_capacity(cw * ch * 3);
        let stride = self.width as usize * 3;
        for row in y0 as usize..y1 as usize {
            let start = row * stride + x0 as usize * 3;
            out.extend_from_slice(&self.data[start..start + cw * 3]);
        }
        Ok(Rgb {
            width: cw as u32,
            height: ch as u32,
            data: out,
        })
    }

    /// Box-filter downscale so the longest edge is at most `max_edge`.
    /// Returns a copy at the same size when already small enough.
    pub fn fit(&self, max_edge: u32) -> Rgb {
        let longest = self.width.max(self.height);
        if longest <= max_edge || max_edge == 0 || longest == 0 {
            return self.clone();
        }
        let scale = max_edge as f64 / longest as f64;
        let nw = ((self.width as f64 * scale).round() as u32).max(1);
        let nh = ((self.height as f64 * scale).round() as u32).max(1);
        self.resize(nw, nh)
    }

    /// Box-filter resize to an exact size.
    pub fn resize(&self, nw: u32, nh: u32) -> Rgb {
        let nw = nw.max(1);
        let nh = nh.max(1);
        let mut out = vec![0u8; nw as usize * nh as usize * 3];
        let sx = self.width as f64 / nw as f64;
        let sy = self.height as f64 / nh as f64;
        let stride = self.width as usize * 3;
        for oy in 0..nh as usize {
            let y0 = (oy as f64 * sy).floor() as usize;
            let y1 = (((oy + 1) as f64 * sy).ceil() as usize)
                .min(self.height as usize)
                .max(y0 + 1);
            for ox in 0..nw as usize {
                let x0 = (ox as f64 * sx).floor() as usize;
                let x1 = (((ox + 1) as f64 * sx).ceil() as usize)
                    .min(self.width as usize)
                    .max(x0 + 1);
                let mut acc = [0u64; 3];
                let mut n = 0u64;
                for y in y0..y1 {
                    let row = y * stride;
                    for x in x0..x1 {
                        let p = row + x * 3;
                        acc[0] += self.data[p] as u64;
                        acc[1] += self.data[p + 1] as u64;
                        acc[2] += self.data[p + 2] as u64;
                        n += 1;
                    }
                }
                let o = (oy * nw as usize + ox) * 3;
                out[o] = (acc[0] / n) as u8;
                out[o + 1] = (acc[1] / n) as u8;
                out[o + 2] = (acc[2] / n) as u8;
            }
        }
        Rgb {
            width: nw,
            height: nh,
            data: out,
        }
    }

    /// The change-detection signature: a 32-pixel-edge thumbnail's bytes.
    pub fn signature(&self) -> Vec<u8> {
        self.fit(SIG_EDGE).data
    }

    /// Encode as PNG.
    pub fn to_png(&self) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, self.width, self.height);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            enc.set_compression(png::Compression::Fast);
            let mut w = enc.write_header().map_err(|e| format!("png header: {e}"))?;
            w.write_image_data(&self.data)
                .map_err(|e| format!("png encode: {e}"))?;
        }
        Ok(out)
    }

    pub fn to_png_base64(&self) -> Result<String, String> {
        Ok(base64::engine::general_purpose::STANDARD.encode(self.to_png()?))
    }
}

/// Thumbnail edge used for the visual-change signature (same as macOS).
const SIG_EDGE: u32 = 32;

/// Decode a PNG of any colour type into 8-bit RGB.
pub fn decode_png(bytes: &[u8]) -> Result<Rgb, String> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder
        .read_info()
        .map_err(|e| format!("png decode: {e}"))?;
    let mut buf = vec![0u8; reader.output_buffer_size().ok_or("png too large")?];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| format!("png frame: {e}"))?;
    buf.truncate(info.buffer_size());
    let (w, h) = (info.width, info.height);
    let n = w as usize * h as usize;
    let data = match info.color_type {
        png::ColorType::Rgb => buf,
        png::ColorType::Rgba => {
            let mut out = Vec::with_capacity(n * 3);
            for px in buf.chunks_exact(4) {
                out.extend_from_slice(&px[..3]);
            }
            out
        }
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g]).collect(),
        png::ColorType::GrayscaleAlpha => buf
            .chunks_exact(2)
            .flat_map(|px| [px[0], px[0], px[0]])
            .collect(),
        other => {
            return Err(format!(
                "unexpected png colour type after expansion: {other:?}"
            ))
        }
    };
    Rgb::new(w, h, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(w: u32, h: u32) -> Rgb {
        let mut data = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h {
            for x in 0..w {
                data.extend_from_slice(&[(x % 256) as u8, (y % 256) as u8, 128]);
            }
        }
        Rgb::new(w, h, data).unwrap()
    }

    #[test]
    fn png_round_trips_pixels_exactly() {
        let img = gradient(37, 23);
        let png = img.to_png().unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let back = decode_png(&png).unwrap();
        assert_eq!(back, img);
        let b64 = img.to_png_base64().unwrap();
        assert!(!b64.is_empty() && !b64.contains('\n'));
    }

    #[test]
    fn rgba_and_grey_pngs_decode_to_rgb() {
        // Encode an RGBA image by hand and make sure alpha is dropped.
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, 2, 1);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&[1, 2, 3, 255, 4, 5, 6, 0]).unwrap();
        }
        let img = decode_png(&out).unwrap();
        assert_eq!(img.data, vec![1, 2, 3, 4, 5, 6]);
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, 1, 1);
            enc.set_color(png::ColorType::Grayscale);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&[9]).unwrap();
        }
        assert_eq!(decode_png(&out).unwrap().data, vec![9, 9, 9]);
    }

    #[test]
    fn junk_is_an_error_not_a_panic() {
        assert!(decode_png(b"").is_err());
        assert!(decode_png(b"\x89PNG\r\n\x1a\nxxxx").is_err());
        assert!(Rgb::new(2, 2, vec![0; 5]).is_err());
    }

    #[test]
    fn crop_clamps_and_rejects_empty_intersections() {
        let img = gradient(10, 10);
        let c = img.crop(2, 3, 4, 5).unwrap();
        assert_eq!((c.width, c.height), (4, 5));
        // Top-left pixel of the crop is the source pixel at (2, 3).
        assert_eq!(&c.data[..3], &[2, 3, 128]);
        // Overhang clamps to the edge.
        let c = img.crop(8, 8, 10, 10).unwrap();
        assert_eq!((c.width, c.height), (2, 2));
        let c = img.crop(-5, -5, 7, 7).unwrap();
        assert_eq!((c.width, c.height), (2, 2));
        assert!(img.crop(10, 0, 5, 5).is_err());
        assert!(img.crop(0, 0, 0, 5).is_err());
        assert!(img.crop(0, 0, -1, -1).is_err());
    }

    #[test]
    fn fit_only_shrinks_and_keeps_aspect() {
        let img = gradient(400, 100);
        let small = img.fit(200);
        assert_eq!((small.width, small.height), (200, 50));
        assert_eq!(img.fit(1000), img);
        assert_eq!(img.fit(0), img);
        let tall = gradient(10, 300).fit(30);
        assert_eq!((tall.width, tall.height), (1, 30));
    }

    #[test]
    fn resize_averages_boxes() {
        let img = Rgb::new(2, 1, vec![0, 0, 0, 200, 100, 50]).unwrap();
        let one = img.resize(1, 1);
        assert_eq!(one.data, vec![100, 50, 25]);
        // Never a zero-sized output.
        assert_eq!((img.resize(0, 0).width, img.resize(0, 0).height), (1, 1));
    }

    #[test]
    fn signature_is_small_and_stable() {
        let img = gradient(640, 480);
        let s1 = img.signature();
        let s2 = img.signature();
        assert_eq!(s1, s2);
        assert_eq!(s1.len(), 32 * 24 * 3);
        // A visibly different frame gives a different signature.
        let mut other = img.clone();
        for px in other.data.chunks_exact_mut(3) {
            px[0] = 255 - px[0];
        }
        assert_ne!(other.signature(), s1);
    }
}
