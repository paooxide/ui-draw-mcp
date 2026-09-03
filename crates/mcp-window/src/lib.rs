//! Window engine: window/app/menu control and the `wait_for` settle primitive
//! (`docs/planning.md` §5.3). OS-independent trait + tool module; the macOS
//! backend lives in `mcp-macos`.

mod backend;
mod tools;
mod wait;

pub use backend::{
    DialogInfo, DialogScope, MenuItemInfo, Rect, WindowAction, WindowBackend, WindowError,
    WindowInfo,
};
pub use tools::WindowModule;
pub use wait::{
    default_wait_timeout, parse_wait_spec, poll_until, text_condition_holds, wait_schema,
    window_condition_holds, WaitCondition, WaitEvaluator, WaitOutcome, WaitSpec, EXPECT_TIMEOUT_MS,
};
