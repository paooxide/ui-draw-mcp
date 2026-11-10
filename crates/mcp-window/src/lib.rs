//! Window engine: window/app/menu control and the `wait_for` settle primitive
//!. OS-independent trait + tool module; the macOS
//! backend lives in `mcp-macos`.

mod backend;
mod foreground;
mod tools;
mod wait;

pub use backend::{
    target_source, window_at, window_index_error, DialogInfo, DialogScope, MenuItemInfo, Rect,
    TargetInfo, TargetSource, WindowAction, WindowBackend, WindowError, WindowInfo, WindowState,
};
pub use foreground::{tool_refusal, ForegroundWindow};
pub use tools::WindowModule;
pub use wait::{
    default_wait_timeout, parse_wait_spec, poll_until, text_condition_holds, wait_schema,
    window_condition_holds, WaitCondition, WaitEvaluator, WaitOutcome, WaitSpec, EXPECT_TIMEOUT_MS,
};
