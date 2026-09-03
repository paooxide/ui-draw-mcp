//! Input engine: semantic (a11y) and coordinate (screen-control) actions —
//! `ui_action`, `set_value`, `keyboard_type`, `keyboard_shortcut`,
//! `mouse_action`, `scroll`, `hover`, `drag_drop`, `clipboard_*`. Shares the
//! `mcp-a11y` snapshot arena so it can act on refs from `get_ui_tree`. Enforces
//! the destructive-input gate and coordinate clamp (`docs/planning.md` §5.2).

mod backend;
mod postcondition;
mod tools;

pub use backend::{
    valid_combo, valid_modifier, ClipData, ClipFormat, InputBackend, InputError, MouseKind,
    ScrollDir, SemanticAction,
};
pub use postcondition::Verifier;
pub use tools::{InputModule, InputPolicy};
