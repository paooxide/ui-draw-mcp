//! Vision capture engine: `list_displays`, `capture_screen`, `capture_window`
//! (`docs/planning.md` §5.1). OS-independent trait + tool module; the macOS
//! backend (CGDisplay + `screencapture`) lives in `mcp-macos`.

mod backend;
mod config;
mod tools;

pub use backend::{CaptureOpts, CaptureResult, Detail, DisplayInfo, VisionBackend, VisionError};
pub use config::VisionConfig;
pub use tools::VisionModule;
