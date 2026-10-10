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
//! anything. A model file is parsed in-process by rten, so every file is
//! checked against a pinned SHA-256 before it is loaded, whether it was just
//! downloaded or was already there: a substituted or modified file is refused
//! and named, never parsed. Loading and recognition block the thread: callers
//! on an async runtime wrap them in `spawn_blocking`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use sha2::{Digest, Sha256};

use crate::backend::OcrLine;

/// What the result reports as `engine`.
pub const ENGINE: &str = "ocrs";

/// A model file: what it is called on disk, where it comes from, and the
/// SHA-256 it must have.
#[derive(Debug, Clone, Copy)]
pub struct ModelFile {
    pub name: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
}

/// The models, pinned by digest.
///
/// The files are the unversioned ones that `ocrs` 0.13.1's own
/// `examples/download-models.sh` fetches from these URLs (the crate names
/// no model version). Digests taken from fresh downloads on 2026-10-10,
/// matching the copies already in use here. Should the upstream files
/// change, the download fails verification and these need updating on
/// purpose, after checking the new files, rather than the server silently
/// parsing whatever the bucket serves.
pub const MODELS: [ModelFile; 2] = [
    ModelFile {
        name: "text-detection.onnx",
        url: "https://ocrs-models.s3-accelerate.amazonaws.com/text-detection.onnx",
        sha256: "a917b23dbd9524b465df7e922641b3ff2981623df4ded5a0234004ef2fee7cfe",
    },
    ModelFile {
        name: "text-recognition.onnx",
        url: "https://ocrs-models.s3-accelerate.amazonaws.com/text-recognition.onnx",
        sha256: "86c145c2edb96c8caed5b1ebb8f44d706408922211451c309c625157dd6061c5",
    },
];

/// Points at a directory of operator-supplied models instead of the state
/// directory. They are still verified against [`MODELS`]'s digests unless
/// [`UNVERIFIED_ENV`] is set.
pub const MODELS_ENV: &str = "AGENTCTL_OCR_MODELS";

/// `=1` lets models in [`MODELS_ENV`]'s directory load without a digest
/// check: for an operator who trained or converted their own. It does
/// nothing for the default directory, whose files the server downloaded.
pub const UNVERIFIED_ENV: &str = "AGENTCTL_OCR_MODELS_UNVERIFIED";

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
        Self::operator_dir().unwrap_or_else(|| state_dir.join("ocr"))
    }

    /// The operator's own model directory, when `$AGENTCTL_OCR_MODELS` names one.
    fn operator_dir() -> Option<PathBuf> {
        std::env::var_os(MODELS_ENV)
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
    }

    /// Both model files, present or fetched.
    pub fn model_paths(dir: &Path) -> [PathBuf; 2] {
        [dir.join(MODELS[0].name), dir.join(MODELS[1].name)]
    }

    pub fn models_present(dir: &Path) -> bool {
        Self::model_paths(dir).iter().all(|p| p.is_file())
    }

    /// Download whichever model is missing, and verify every model, present
    /// or fetched, before anything parses it. Blocking: it runs `curl`.
    ///
    /// Safe to race: a process downloads one model at a time, each download
    /// goes to a file named for its process, is verified there, and is only
    /// then renamed into place; a model that appeared meanwhile (another
    /// process got there first) is verified and kept rather than fetched
    /// again.
    fn fetch_and_verify(dir: &Path) -> Result<(), String> {
        let _one_at_a_time = FETCH_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let operator = Self::operator_dir().is_some_and(|d| d == dir);
        let unverified = operator && std::env::var_os(UNVERIFIED_ENV).is_some_and(|v| v == "1");
        std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        for m in MODELS {
            let dest = dir.join(m.name);
            if dest.is_file() {
                if unverified {
                    tracing::warn!(file = %dest.display(), "loading OCR model without verification ({UNVERIFIED_ENV}=1)");
                    continue;
                }
                // A file the operator put here is theirs to replace; one the
                // server downloaded is deleted so the next call fetches afresh.
                verify_file(&dest, m.sha256, !operator).map_err(|e| {
                    if operator {
                        format!(
                            "{e}; replace it with the published model, or set {UNVERIFIED_ENV}=1                              to load models of your own from {MODELS_ENV}"
                        )
                    } else {
                        format!("{e}; it was deleted, and the next call downloads it again")
                    }
                })?;
                continue;
            }
            tracing::info!(url = m.url, dest = %dest.display(), "downloading OCR model (first use)");
            let tmp = dir.join(format!("{}.{}.part", m.name, std::process::id()));
            let out = std::process::Command::new("/usr/bin/curl")
                .args(["-fsSL", "--max-time", "300", "-o"])
                .arg(&tmp)
                .arg(m.url)
                .output()
                .map_err(|e| {
                    format!(
                        "curl: {e} (place {} in {} by hand to skip the download)",
                        m.name,
                        dir.display()
                    )
                })?;
            if !out.status.success() {
                let _ = std::fs::remove_file(&tmp);
                return Err(format!(
                    "downloading {} failed: {} (place {} in {} by hand to skip the download)",
                    m.url,
                    String::from_utf8_lossy(&out.stderr).trim(),
                    m.name,
                    dir.display()
                ));
            }
            // Verified before it gets its real name: a bad download never
            // sits where the next call would trust it.
            verify_file(&tmp, m.sha256, true)
                .map_err(|e| format!("{e}; the download was discarded"))?;
            if let Err(e) = std::fs::rename(&tmp, &dest) {
                let _ = std::fs::remove_file(&tmp);
                if !dest.is_file() {
                    return Err(format!("moving {}: {e}", tmp.display()));
                }
                // Another process won the race: its file is checked too.
                if !unverified {
                    verify_file(&dest, m.sha256, !operator)?;
                }
            }
        }
        Ok(())
    }

    /// Load the models from `dir`, fetching them first if they are missing
    /// and verifying each against its pinned digest. Blocking: a few hundred
    /// milliseconds when present, longer to download.
    pub fn load(dir: &Path) -> Result<Ocr, String> {
        Self::fetch_and_verify(dir)?;
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

/// The hex SHA-256 of a file, streamed.
fn sha256_file(path: &Path) -> Result<String, String> {
    let mut f =
        std::fs::File::open(path).map_err(|e| format!("opening {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| format!("reading {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Check `path` against `expected` (hex SHA-256). On a mismatch the file is
/// removed when `delete` is set, and the error names the file and both
/// digests, so the operator sees what was refused and why.
fn verify_file(path: &Path, expected: &str, delete: bool) -> Result<(), String> {
    let got = sha256_file(path)?;
    if got.eq_ignore_ascii_case(expected) {
        return Ok(());
    }
    if delete {
        let _ = std::fs::remove_file(path);
    }
    Err(format!(
        "{} failed verification: sha256 {got}, expected {expected}",
        path.display()
    ))
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
        if std::env::var_os(MODELS_ENV).is_none() {
            assert_eq!(Ocr::model_dir(&dir), dir.join("ocr"));
        }
        let [a, b] = Ocr::model_paths(&dir);
        assert!(a.ends_with("text-detection.onnx"));
        assert!(b.ends_with("text-recognition.onnx"));
    }

    const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agentctl-ocr-verify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    /// The pinned digests are well-formed, so a typo cannot make every
    /// download fail verification.
    #[test]
    fn pinned_digests_are_hex_sha256() {
        for m in MODELS {
            assert_eq!(m.sha256.len(), 64, "{}", m.name);
            assert!(
                m.sha256.chars().all(|c| c.is_ascii_hexdigit()),
                "{}",
                m.name
            );
            assert!(m.url.starts_with("https://"), "{}", m.name);
        }
        assert_ne!(MODELS[0].sha256, MODELS[1].sha256);
    }

    /// A file with the expected content passes and stays; the digest is
    /// computed by streaming, so it is the same as `sha256sum`'s.
    #[test]
    fn a_file_with_the_pinned_content_passes_and_is_kept() {
        let p = scratch("good.onnx");
        std::fs::write(&p, b"hello").unwrap();
        assert_eq!(sha256_file(&p).unwrap(), HELLO_SHA256);
        verify_file(&p, HELLO_SHA256, true).unwrap();
        verify_file(&p, &HELLO_SHA256.to_uppercase(), true).unwrap();
        assert!(p.is_file(), "a verified file must not be touched");
    }

    /// Wrong content is refused, named, and removed when asked, so a bad
    /// download never sits where the next call would trust it; an
    /// operator's file is refused but left for them to replace.
    #[test]
    fn a_file_with_other_content_is_refused_and_removed() {
        let p = scratch("bad.onnx");
        std::fs::write(&p, b"hello, tampered").unwrap();
        let err = verify_file(&p, HELLO_SHA256, true).unwrap_err();
        assert!(err.contains("bad.onnx"), "{err}");
        assert!(err.contains("failed verification"), "{err}");
        assert!(err.contains(HELLO_SHA256), "{err}");
        assert!(!p.exists(), "a refused download must be deleted");

        let p = scratch("operator.onnx");
        std::fs::write(&p, b"hello, tampered").unwrap();
        assert!(verify_file(&p, HELLO_SHA256, false).is_err());
        assert!(
            p.is_file(),
            "an operator-supplied file is theirs to replace"
        );
    }

    /// A present-but-wrong model in the default directory is refused before
    /// anything parses it, with the way out in the message, and no network
    /// is touched: the file is gone afterwards, which is the "clear it" step
    /// done for the operator.
    #[test]
    fn a_tampered_model_in_the_state_directory_is_refused_before_load() {
        if std::env::var_os(MODELS_ENV).is_some() {
            return; // the override makes every dir the operator's
        }
        let dir = scratch("state").join("ocr");
        std::fs::create_dir_all(&dir).unwrap();
        // Both present so no download is attempted; the first is wrong.
        for m in MODELS {
            std::fs::write(dir.join(m.name), b"not a model").unwrap();
        }
        let err = match Ocr::load(&dir) {
            Ok(_) => panic!("a tampered model must not load"),
            Err(e) => e,
        };
        assert!(err.contains(MODELS[0].name), "{err}");
        assert!(err.contains("failed verification"), "{err}");
        assert!(err.contains("deleted"), "{err}");
        assert!(!dir.join(MODELS[0].name).exists());
    }
}
