//! Input engine: semantic (a11y) and coordinate (screen-control) actions —
//! `ui_action`, `set_value`, `keyboard_type`, `keyboard_shortcut`,
//! `mouse_action`, `scroll`, `hover`, `drag_drop`, `select_text`, `get_selection`,
//! `clipboard_*`. Shares the
//! `mcp-a11y` snapshot arena so it can act on refs from `get_ui_tree`. Enforces
//! the destructive-input gate and coordinate clamp.

mod backend;
mod choose;
mod combo;
mod effects;
mod foreground;
pub mod glide;
mod hold;
mod human_override;
mod pid_route;
mod postcondition;
mod selection;
mod steps;
mod tools;

pub use backend::{
    valid_modifier, BackgroundTarget, Choice, ClipData, ClipFormat, FocusedText, InputBackend,
    InputError, MouseKind, Reading, Route, ScrollDir, Selection, SemanticAction,
};
pub use choose::{list_options, missing_message, pick_option, same_option, OptionItem, Pick};
pub use combo::{parse_combo, parse_hold, Combo, Os, KEY_NAMES};
pub use effects::{diff_facts, ElementFacts, Focus, Overlay, UiFacts, UiProbe};
pub use foreground::{tool_refusal, ForegroundInput};
pub use glide::{bezier_interpolate, execute_glide, GlideConfig, GlidePreset};
pub use hold::{HeldKeys, HOLD_MAX_MS, MAX_HELD_KEYS};
pub use human_override::{Activity, ActivityGuard, Detector, OverrideConfig, SetPoint, Verdict};
pub use postcondition::Verifier;
pub use selection::{char_range_to_utf16, char_slice, utf16_range_to_char};
pub use steps::{parse_steps, Step, StepDispatch, StepRelay, BATCHABLE, MAX_BATCH_MS, MAX_STEPS};
pub use tools::{InputModule, InputPolicy};
