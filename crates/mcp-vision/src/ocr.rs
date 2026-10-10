//! The pure-Rust text recogniser: `ocrs` on `rten`, run in-process on the CPU.
//!
//! Platform OCR frameworks exist on macOS (Vision) and nowhere else, so this
//! is what the Linux desktop backend reads text with, and what the browser
//! engine reads text with on every platform, since a page's pixels do not
//! come from the screen and a Chrome on macOS is the same Chrome as on Linux.
//! Keeping the engine here, behind the `ocr` feature, means one copy of the
//! model handling and one model download shared by both.
//!
//! The two models (about 20 MB) are downloaded on first use into the model
//! directory, the way the macOS backend compiles its helper on first use, and
//! can be placed there ahead of time on a machine that must not fetch
//! anything. Loading and recognition block the thread: callers on an async
//! runtime wrap them in `spawn_blocking`.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::backend::OcrLine;

/// What the result reports as `engine`.
pub const ENGINE: &str = "ocrs";

/// Where the models come from, and what they are called on disk.
pub const MODEL_URLS: [(&str, &str); 2] = [
    (
        "text-detection.onnx",
        "https://ocrs-models.s3-accelerate.amazonaws.com/text-detection.onnx",
    ),
    (
        "text-recognition.onnx",
        "https://ocrs-models.s3-accelerate.amazonaws.com/text-recognition.onnx",
    ),
];

/// Words on one row further apart than this many row heights are separate
/// lines. The recogniser groups by row, so two buttons side by side come back
/// as "Save Cancel" with one box, and a `find` for either gets a box estimated
/// by character count that lands between them. A word space is about a third
/// of the row height; a whole row height of nothing is a gap between things.
const WIDE_GAP_ROW_HEIGHTS: f64 = 1.0;

/// One download at a time per process: two engines loading at once (two
/// modules in one server, or two tests) must not both write the same file.
static FETCH_LOCK: Mutex<()> = Mutex::new(());

/// The loaded recogniser.
pub struct Ocr {
    engine: ::ocrs::OcrEngine,
}

/// A recognised word: its text and `(left, top, right, bottom)` box in image
/// pixels, from the recogniser's per-character rectangles.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Word {
    text: String,
    l: i32,
    t: i32,
    r: i32,
    b: i32,
}

/// Split a recognised row into the pieces a reader sees as separate: words
/// closer than [`WIDE_GAP_ROW_HEIGHTS`] rows stay together, a wider gap starts
/// a new piece. Each piece is a line with a box that is the union of its
/// words' boxes, so a label on a canvas gets its own clickable centre even
/// when another label shares its row.
fn segment_row(words: &[Word]) -> Vec<OcrLine> {
    let mut out: Vec<OcrLine> = Vec::new();
    let mut group: Vec<&Word> = Vec::new();
    let flush = |group: &mut Vec<&Word>, out: &mut Vec<OcrLine>| {
        if group.is_empty() {
            return;
        }
        let l = group.iter().map(|w| w.l).min().unwrap_or(0);
        let t = group.iter().map(|w| w.t).min().unwrap_or(0);
        let r = group.iter().map(|w| w.r).max().unwrap_or(0);
        let b = group.iter().map(|w| w.b).max().unwrap_or(0);
        let text = group
            .iter()
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        out.push(OcrLine {
            text,
            confidence: 1.0,
            px: (
                l as f64,
                t as f64,
                (r - l).max(1) as f64,
                (b - t).max(1) as f64,
            ),
        });
        group.clear();
    };
    for w in words {
        if let Some(prev) = group.last() {
            let row_h = (prev.b - prev.t).max(w.b - w.t).max(1) as f64;
            let gap = (w.l - prev.r) as f64;
            if gap > row_h * WIDE_GAP_ROW_HEIGHTS {
                flush(&mut group, &mut out);
            }
        }
        group.push(w);
    }
    flush(&mut group, &mut out);
    out
}

impl Ocr {
    /// Where the models live: `$AGENTCTL_OCR_MODELS` when set (a shared or
    /// pre-populated directory), else `<state>/ocr`.
    pub fn model_dir(state_dir: &Path) -> PathBuf {
        std::env::var_os("AGENTCTL_OCR_MODELS")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| state_dir.join("ocr"))
    }

    /// Both model files, present or fetched.
    pub fn model_paths(dir: &Path) -> [PathBuf; 2] {
        [dir.join(MODEL_URLS[0].0), dir.join(MODEL_URLS[1].0)]
    }

    pub fn models_present(dir: &Path) -> bool {
        Self::model_paths(dir).iter().all(|p| p.is_file())
    }

    /// Download whichever model is missing. Blocking: it runs `curl`.
    ///
    /// Safe to race: a process downloads one model at a time, each download
    /// goes to a file named for its process and is renamed into place, and a
    /// model that appeared meanwhile (another process got there first) is
    /// kept rather than fetched again.
    fn fetch(dir: &Path) -> Result<(), String> {
        let _one_at_a_time = FETCH_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        for (name, url) in MODEL_URLS {
            let dest = dir.join(name);
            if dest.is_file() {
                continue;
            }
            tracing::info!(url, dest = %dest.display(), "downloading OCR model (first use)");
            let tmp = dir.join(format!("{name}.{}.part", std::process::id()));
            let out = std::process::Command::new("/usr/bin/curl")
                .args(["-fsSL", "--max-time", "300", "-o"])
                .arg(&tmp)
                .arg(url)
                .output()
                .map_err(|e| {
                    format!(
                        "curl: {e} (place {name} in {} by hand to skip the download)",
                        dir.display()
                    )
                })?;
            if !out.status.success() {
                let _ = std::fs::remove_file(&tmp);
                return Err(format!(
                    "downloading {url} failed: {} (place {name} in {} by hand to skip the download)",
                    String::from_utf8_lossy(&out.stderr).trim(),
                    dir.display()
                ));
            }
            if let Err(e) = std::fs::rename(&tmp, &dest) {
                let _ = std::fs::remove_file(&tmp);
                if !dest.is_file() {
                    return Err(format!("moving {}: {e}", tmp.display()));
                }
            }
        }
        Ok(())
    }

    /// Load the models from `dir`, fetching them first if they are missing.
    /// Blocking: a few hundred milliseconds when present, longer to download.
    pub fn load(dir: &Path) -> Result<Ocr, String> {
        if !Self::models_present(dir) {
            Self::fetch(dir)?;
        }
        let [det, rec] = Self::model_paths(dir);
        let detection_model = rten::Model::load_file(&det)
            .map_err(|e| format!("loading {}: {e} (delete it to re-download)", det.display()))?;
        let recognition_model = rten::Model::load_file(&rec)
            .map_err(|e| format!("loading {}: {e} (delete it to re-download)", rec.display()))?;
        let engine = ::ocrs::OcrEngine::new(::ocrs::OcrEngineParams {
            detection_model: Some(detection_model),
            recognition_model: Some(recognition_model),
            ..Default::default()
        })
        .map_err(|e| format!("ocr engine: {e}"))?;
        tracing::info!(dir = %dir.display(), "OCR models loaded");
        Ok(Ocr { engine })
    }

    /// Recognise text lines in an 8-bit image given as row-major pixels with
    /// 1 (grey), 3 (RGB) or 4 (RGBA) channels. Boxes are image pixels.
    ///
    /// A row the recogniser read as one line is split where words are a row
    /// height or more apart (see [`segment_row`]), so labels side by side on
    /// a toolbar or a canvas come back one each.
    ///
    /// `ocrs` reports no per-line probability, so `confidence` is 1.0 for
    /// every line it returns; a `min_confidence` above that filters
    /// everything, which is the honest outcome of asking for a number the
    /// engine does not have.
    pub fn recognise(
        &self,
        pixels: &[u8],
        width: u32,
        height: u32,
    ) -> Result<Vec<OcrLine>, String> {
        use ::ocrs::{ImageSource, TextItem};
        let source = ImageSource::from_bytes(pixels, (width, height))
            .map_err(|e| format!("ocr input: {e}"))?;
        let input = self
            .engine
            .prepare_input(source)
            .map_err(|e| format!("ocr prepare: {e}"))?;
        let words = self
            .engine
            .detect_words(&input)
            .map_err(|e| format!("ocr detect: {e}"))?;
        let lines = self.engine.find_text_lines(&input, &words);
        let texts = self
            .engine
            .recognize_text(&input, &lines)
            .map_err(|e| format!("ocr recognise: {e}"))?;
        let mut out = Vec::new();
        for line in texts.into_iter().flatten() {
            let words: Vec<Word> = line
                .words()
                .filter_map(|w| {
                    let text = w.to_string();
                    if text.trim().is_empty() {
                        return None;
                    }
                    let chars = w.chars();
                    Some(Word {
                        text,
                        l: chars.iter().map(|c| c.rect.left()).min().unwrap_or(0),
                        t: chars.iter().map(|c| c.rect.top()).min().unwrap_or(0),
                        r: chars.iter().map(|c| c.rect.right()).max().unwrap_or(0),
                        b: chars.iter().map(|c| c.rect.bottom()).max().unwrap_or(0),
                    })
                })
                .collect();
            out.extend(segment_row(&words));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(text: &str, l: i32, r: i32) -> Word {
        Word {
            text: text.into(),
            l,
            t: 100,
            r,
            b: 120,
        }
    }

    /// Two labels on one row, a row height or more apart, are two lines with
    /// their own boxes; the words of one label stay together.
    #[test]
    fn a_row_splits_at_wide_gaps_and_keeps_word_spaces() {
        // "Save" | 160 px of nothing | "Open file" with a 6 px word space.
        let words = [
            word("Save", 20, 80),
            word("Open", 240, 300),
            word("file", 306, 350),
        ];
        let lines = segment_row(&words);
        let texts: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["Save", "Open file"]);
        assert_eq!(lines[0].px, (20.0, 100.0, 60.0, 20.0));
        assert_eq!(lines[1].px, (240.0, 100.0, 110.0, 20.0));
    }

    /// A gap just under a row height is still one line: that is a wide word
    /// space or a tab stop, not another element.
    #[test]
    fn a_gap_under_a_row_height_does_not_split() {
        let words = [word("Save", 20, 80), word("As", 99, 120)];
        assert_eq!(segment_row(&words).len(), 1);
        let words = [word("Save", 20, 80), word("As", 101, 120)];
        assert_eq!(segment_row(&words).len(), 2);
    }

    #[test]
    fn an_empty_row_is_no_lines() {
        assert!(segment_row(&[]).is_empty());
    }

    #[test]
    fn model_paths_are_stable_and_absent_by_default() {
        let dir = std::env::temp_dir().join(format!("agentctl-ocr-{}", std::process::id()));
        assert!(!Ocr::models_present(&dir));
        // Without the override the models sit under the state directory.
        if std::env::var_os("AGENTCTL_OCR_MODELS").is_none() {
            assert_eq!(Ocr::model_dir(&dir), dir.join("ocr"));
        }
        let [a, b] = Ocr::model_paths(&dir);
        assert!(a.ends_with("text-detection.onnx"));
        assert!(b.ends_with("text-recognition.onnx"));
    }
}
