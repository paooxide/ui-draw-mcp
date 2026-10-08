//! Input engine: semantic (a11y) and coordinate (screen-control) actions —
//! `ui_action`, `set_value`, `keyboard_type`, `keyboard_shortcut`,
//! `mouse_action`, `scroll`, `hover`, `drag_drop`, `clipboard_*`. Shares the
//! `mcp-a11y` snapshot arena so it can act on refs from `get_ui_tree`. Enforces
//! the destructive-input gate and coordinate clamp.

mod backend;
mod choose;
mod combo;
pub mod glide;
mod human_override;
mod postcondition;
mod tools;

pub use backend::{
    valid_modifier, Choice, ClipData, ClipFormat, InputBackend, InputError, MouseKind, Reading,
    ScrollDir, SemanticAction,
};
pub use choose::{list_options, missing_message, pick_option, same_option, OptionItem, Pick};
pub use combo::{parse_combo, Combo, Os, KEY_NAMES};
pub use glide::{bezier_interpolate, execute_glide, GlideConfig, GlidePreset};
pub use human_override::{Activity, ActivityGuard, Detector, OverrideConfig, SetPoint, Verdict};
pub use postcondition::Verifier;
pub use tools::{InputModule, InputPolicy};
