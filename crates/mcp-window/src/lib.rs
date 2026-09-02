//! Window engine: window/app/menu control and the `wait_for` settle primitive
//! (`docs/planning.md` §5.3). OS-independent trait + tool module; the macOS
//! backend lives in `mcp-macos`.

mod backend;
mod tools;

pub use backend::{
    DialogInfo, DialogScope, MenuItemInfo, Rect, WindowAction, WindowBackend, WindowError,
    WindowInfo,
};
pub use tools::WindowModule;
