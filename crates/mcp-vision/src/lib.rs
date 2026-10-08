//! Vision capture engine: `list_displays`, `capture_screen`, `capture_window`
//!. OS-independent trait + tool module; the macOS
//! backend (CGDisplay + `screencapture`) lives in `mcp-macos`.

mod backend;
mod config;
mod find;
pub mod grid;
mod tools;

pub use backend::{
    CaptureOpts, CaptureResult, Detail, DisplayInfo, OcrLine, OcrOpts, OcrResult, OcrTarget,
    VisionBackend, VisionError,
};
pub use config::VisionConfig;
pub use tools::VisionModule;
