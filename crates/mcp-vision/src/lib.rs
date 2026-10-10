//! Vision capture engine: `list_displays`, `capture_screen`, `capture_window`
//!. OS-independent trait + tool module; the macOS
//! backend (CGDisplay + `screencapture`) lives in `mcp-macos`.
//!
//! Two pieces are platform-neutral and shared with the browser engine: the
//! coordinate grid (`grid`) and the OCR text search (`find`). With the `ocr`
//! feature the pure-Rust `ocrs` recogniser is here too (`ocr`), so a backend
//! without a platform OCR framework, and `browser_screenshot` on every
//! platform, read text with the same engine and the same models.

mod backend;
mod config;
pub mod find;
pub mod grid;
#[cfg(feature = "ocr")]
pub mod ocr;
mod tools;

pub use backend::{
    CaptureOpts, CaptureResult, Detail, DisplayInfo, OcrLine, OcrOpts, OcrResult, OcrTarget,
    VisionBackend, VisionError,
};
pub use config::VisionConfig;
pub use tools::VisionModule;
