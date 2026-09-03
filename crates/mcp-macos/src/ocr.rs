//! Text recognition through Apple's Vision framework, without linking it.
//!
//! `docs/planning.md` deferred `ocr_region` because the alternatives were a
//! heavy Rust binding or a build that depends on the Xcode toolchain. There is
//! a third option, and it is the one this codebase already uses for screen
//! capture: shell out. A ~90-line Swift program does the recognition; this
//! module compiles it on first use and runs it.
//!
//! Compiling lazily rather than in a build script is deliberate. `cargo build`
//! must not fail on a machine without the Command Line Tools, and a machine
//! without them should get a clear `UNSUPPORTED_OS` naming the fix at call
//! time. The compiled binary is cached under the agentctl state directory,
//! keyed by a hash of the source, so changing the Swift recompiles it and
//! nothing else does.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use mcp_vision::{OcrLine, VisionError};
use serde_json::Value;

/// The helper's source, compiled on demand.
const SOURCE: &str = include_str!("../helpers/ocr.swift");

/// Serialises first-use compilation: two concurrent calls must not both build
/// into the same path.
static COMPILE_LOCK: Mutex<()> = Mutex::new(());

/// FNV-1a. Enough to notice the source changed; nothing here is adversarial.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Where the compiled helper for the current source lives.
pub fn helper_path(dir: &Path) -> PathBuf {
    dir.join(format!("agentctl-ocr-{:016x}", fnv1a64(SOURCE.as_bytes())))
}

/// Ensure the helper exists, compiling it if this is the first use.
pub fn ensure_helper(dir: &Path) -> Result<PathBuf, VisionError> {
    let out = helper_path(dir);
    if out.exists() {
        return Ok(out);
    }
    let _guard = COMPILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if out.exists() {
        return Ok(out);
    }

    // Preflight. Running `swiftc` when the tools are missing pops the macOS
    // "install command line developer tools?" dialog — a modal box in front of
    // whoever is at the machine, caused by an agent's tool call. Ask first.
    let probe = std::process::Command::new("/usr/bin/xcode-select")
        .arg("-p")
        .output();
    let have_tools = matches!(probe, Ok(o) if o.status.success());
    if !have_tools {
        return Err(VisionError::Unsupported(
            "text recognition needs the Xcode Command Line Tools; \
             install them with `xcode-select --install`"
                .into(),
        ));
    }

    std::fs::create_dir_all(dir)
        .map_err(|e| VisionError::Failed(format!("could not create {}: {e}", dir.display())))?;
    let src = dir.join(format!(
        "agentctl-ocr-{:016x}.swift",
        fnv1a64(SOURCE.as_bytes())
    ));
    std::fs::write(&src, SOURCE)
        .map_err(|e| VisionError::Failed(format!("could not write the helper source: {e}")))?;

    // Build to a temporary name and rename, so a half-written binary is never
    // at the path the existence check trusts.
    let tmp = dir.join(format!("agentctl-ocr-{}.tmp", std::process::id()));
    tracing::info!("compiling the OCR helper (first use; a few seconds)");
    let status = std::process::Command::new("/usr/bin/xcrun")
        .args(["swiftc", "-O", "-o"])
        .arg(&tmp)
        .arg(&src)
        .output()
        .map_err(|e| VisionError::Failed(format!("could not run swiftc: {e}")))?;
    if !status.status.success() {
        let err = String::from_utf8_lossy(&status.stderr);
        let _ = std::fs::remove_file(&tmp);
        return Err(VisionError::Failed(format!(
            "could not compile the OCR helper: {}",
            err.lines().take(3).collect::<Vec<_>>().join("; ")
        )));
    }
    std::fs::rename(&tmp, &out)
        .map_err(|e| VisionError::Failed(format!("could not install the OCR helper: {e}")))?;
    let _ = std::fs::remove_file(&src);
    Ok(out)
}

/// Parse the helper's JSON. Split out so the shape is testable without running
/// anything.
pub fn parse_helper_json(
    text: &str,
    min_confidence: f64,
) -> Result<(Vec<OcrLine>, u32, u32), VisionError> {
    let v: Value = serde_json::from_str(text)
        .map_err(|e| VisionError::Failed(format!("OCR helper returned malformed JSON: {e}")))?;
    let width = v.get("width").and_then(Value::as_u64).unwrap_or(0) as u32;
    let height = v.get("height").and_then(Value::as_u64).unwrap_or(0) as u32;
    let mut lines = Vec::new();
    for l in v.get("lines").and_then(Value::as_array).unwrap_or(&vec![]) {
        let confidence = l.get("confidence").and_then(Value::as_f64).unwrap_or(0.0);
        if confidence < min_confidence {
            continue;
        }
        let Some(text) = l.get("text").and_then(Value::as_str) else {
            continue;
        };
        let b = l.get("box");
        let f = |k: &str| {
            b.and_then(|b| b.get(k))
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
        };
        lines.push(OcrLine {
            text: text.to_string(),
            confidence,
            px: (f("x"), f("y"), f("w"), f("h")),
        });
    }
    Ok((lines, width, height))
}

/// Recognise text in a PNG on disk.
pub async fn ocr_png(
    helper: &Path,
    png: &Path,
    opts: &mcp_vision::OcrOpts,
) -> Result<(Vec<OcrLine>, u32, u32), VisionError> {
    let mut cmd = tokio::process::Command::new(helper);
    cmd.arg(png);
    if !opts.languages.is_empty() {
        cmd.arg("--lang").arg(opts.languages.join(","));
    }
    if opts.fast {
        cmd.arg("--fast");
    }
    let out = tokio::time::timeout(std::time::Duration::from_secs(30), cmd.output())
        .await
        .map_err(|_| VisionError::Failed("OCR timed out after 30s".into()))?
        .map_err(|e| VisionError::Failed(format!("could not run the OCR helper: {e}")))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(VisionError::Failed(format!(
            "OCR failed: {}",
            err.trim().lines().next().unwrap_or("unknown error")
        )));
    }
    parse_helper_json(&String::from_utf8_lossy(&out.stdout), opts.min_confidence)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hash_is_stable_and_distinguishes_sources() {
        assert_eq!(fnv1a64(b"agentctl"), fnv1a64(b"agentctl"));
        assert_ne!(fnv1a64(b"agentctl"), fnv1a64(b"agentctm"));
        // The cache path must change when the source does, or an edit to the
        // Swift would keep running the old binary forever.
        let dir = Path::new("/tmp");
        assert!(helper_path(dir)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("agentctl-ocr-"));
    }

    #[test]
    fn helper_output_parses_into_lines() {
        let json = r#"{"width":900,"height":120,"lines":[
            {"text":"AGENTCTL OCR 12345","confidence":1.0,"box":{"x":4.0,"y":2.0,"w":136.0,"h":12.0}},
            {"text":"blurry","confidence":0.2,"box":{"x":1.0,"y":20.0,"w":10.0,"h":10.0}}]}"#;
        let (lines, w, h) = parse_helper_json(json, 0.0).unwrap();
        assert_eq!((w, h), (900, 120));
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "AGENTCTL OCR 12345");
        assert_eq!(lines[0].px, (4.0, 2.0, 136.0, 12.0));
    }

    /// A recogniser that is unsure is often wrong, and a wrong line an agent
    /// acts on is worse than a missing one.
    #[test]
    fn low_confidence_lines_are_dropped() {
        let json = r#"{"width":10,"height":10,"lines":[
            {"text":"sure","confidence":0.9,"box":{"x":0,"y":0,"w":1,"h":1}},
            {"text":"guess","confidence":0.1,"box":{"x":0,"y":0,"w":1,"h":1}}]}"#;
        let (lines, _, _) = parse_helper_json(json, 0.5).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "sure");
    }

    #[test]
    fn malformed_output_is_an_error_not_a_panic() {
        assert!(parse_helper_json("not json", 0.0).is_err());
        // Missing fields are tolerated: a helper that returns an empty result
        // is saying "no text", which is a legitimate answer.
        let (lines, w, _) = parse_helper_json("{}", 0.0).unwrap();
        assert!(lines.is_empty());
        assert_eq!(w, 0);
    }
}
