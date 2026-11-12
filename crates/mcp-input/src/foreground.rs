//! `foreground = "never"` for the input engine: which calls are refused, what
//! the refusal says, and the backend wrapper that makes the refusal hold.
//!
//! Two layers, because they answer different questions.
//!
//! * The tool layer ([`tool_refusal`], called first in `InputModule::call`)
//!   knows the tool and its arguments, so it can refuse before anything else
//!   runs (no snapshot, no consent dialog for a typed command, no half-filled
//!   form) and can name the tool that does the same job in the background.
//! * The backend layer ([`ForegroundInput`]) is the one that cannot be
//!   bypassed: every synthetic-input method of the real backend sits behind
//!   it, so a tool added later, or a path that reaches the backend some other
//!   way, still posts nothing. It exists because the route an action took is
//!   only known after it ran, and by then the pointer has moved.
//!
//! When unsure whether an accessibility action brings an app forward, it is
//! refused here and the doubt is said in the refusal. An action is let through
//! only where it has been measured not to (see [`focus_is_quiet_on`]).

use std::sync::Arc;

use async_trait::async_trait;
use mcp_types::Refusal;
use serde_json::Value;

use crate::backend::{
    Choice, ClipData, ClipFormat, InputBackend, InputError, MouseKind, Reading, Route, ScrollDir,
    Selection, SemanticAction,
};

/// How a tool stands under `foreground = "never"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stance {
    /// Never posts synthetic input and never raises anything.
    Background,
    /// Always posts synthetic input or needs the pointer.
    Refused,
    /// Depends on the arguments: see [`by_args`].
    ByArgs,
}

/// Every tool of the input engine and where it stands. A test requires this to
/// list exactly the tools the module advertises, so a new tool cannot ship
/// without somebody deciding; until then [`tool_refusal`] refuses it.
pub(crate) const TOOLS: &[(&str, Stance)] = &[
    ("ui_action", Stance::ByArgs),
    ("set_value", Stance::Background),
    ("select_text", Stance::Background),
    ("get_selection", Stance::Background),
    ("ui_fill_form", Stance::ByArgs),
    ("keyboard_type", Stance::ByArgs),
    ("keyboard_shortcut", Stance::ByArgs),
    ("mouse_action", Stance::Refused),
    ("scroll", Stance::Refused),
    ("hover", Stance::Refused),
    ("drag_drop", Stance::Refused),
    // The clipboard is shared state, not input: nothing here moves the
    // pointer or touches the foreground.
    ("clipboard_read", Stance::Background),
    ("clipboard_write", Stance::Background),
    ("input_showcase", Stance::Refused),
    // A batch is as background-safe as its least safe step: see `by_args`.
    ("input_steps", Stance::ByArgs),
];

const POINTER: &str = "move the real pointer";
const KEYS: &str = "post synthetic key events to whichever app is frontmost, after first bringing \
                    the target app to the front";
const UNVERIFIED_FOCUS: &str = "ask an app to take keyboard focus, which an app may answer by \
                                coming to the front (not verified to be background-safe on this \
                                platform, so it is refused)";

/// Whether `ui_action focus` has been watched not to bring an app forward on
/// `platform`.
///
/// Measured on macOS by `measured_which_refused_operations_would_have_raised_the_app`
/// in `agentctl/tests/live_desktop_input.rs`: with Finder frontmost, setting
/// `AXFocused` on TextEdit's text area was run three times and each time left
/// both the topmost window's owner and the active application on Finder, with
/// no menu on screen. Nothing was measured on Linux, where the same action is
/// AT-SPI `grab_focus` and a compositor may answer it by raising the window, so
/// it stays refused there.
fn focus_is_quiet_on(platform: &str) -> bool {
    platform == "macos"
}

/// Whether per-process key delivery has been watched leave the frontmost app,
/// the pointer and the windows alone on `platform`.
///
/// Measured on macOS by `pid_event_types_into_a_background_app_without_raising_it`
/// in `agentctl/tests/live_desktop_input.rs`: with Finder frontmost and a
/// TextEdit document behind it, keys sent to TextEdit's process arrived (text
/// and text-system chords; a menu key equivalent such as Command-A did not, and
/// the tool says so) while the topmost window's owner, the active application,
/// Finder's focused element and the pointer were all unchanged. Linux has no
/// per-process delivery at all, so the route does not exist there.
fn pid_route_is_quiet_on(platform: &str) -> bool {
    platform == "macos"
}

/// Whether keys would go to the pinned app by process rather than to the
/// frontmost one, which is what makes them safe under `never`.
fn keys_go_to_background(backend: &dyn InputBackend) -> bool {
    pid_route_is_quiet_on(backend.platform()) && backend.background_key_target().is_some()
}

const UNVERIFIED_MENU: &str = "open a menu or popup in another app, which may bring that app \
                               forward (not verified to be background-safe, so it is refused)";

fn aimed(args: &Value) -> bool {
    args.get("ref").is_some_and(|v| !v.is_null())
        || args
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|n| !n.trim().is_empty())
}

fn snake(s: &str) -> String {
    s.trim().to_lowercase().replace(['-', ' '], "_")
}

/// The refusal for `tool` called with `args`, or `None` when it may run.
///
/// Pure: it reads nothing but its arguments and what `backend` says about the
/// route an accessibility action takes on its platform.
pub fn tool_refusal(backend: &dyn InputBackend, tool: &str, args: &Value) -> Option<Refusal> {
    match TOOLS.iter().find(|(n, _)| *n == tool).map(|(_, s)| *s) {
        Some(Stance::Background) => None,
        Some(Stance::Refused) => Some(always_refused(backend, tool, args)),
        Some(Stance::ByArgs) => by_args(backend, tool, args),
        None => Some(Refusal::new(
            tool,
            "do something this server has not classified as background-safe",
            None,
        )),
    }
}

fn always_refused(backend: &dyn InputBackend, tool: &str, args: &Value) -> Refusal {
    let kind = args
        .get("type")
        .and_then(Value::as_str)
        .map(snake)
        .unwrap_or_default();
    match tool {
        "keyboard_type" => Refusal::new(
            tool,
            KEYS,
            Some(if aimed(args) {
                "set_value writes the field's value through accessibility; the field need not \
                 be focused or frontmost, and the text replaces what it holds. Or pin the app \
                 with focus_app (activate false) while it is in the background: keys are then \
                 sent to that process without raising it"
            } else {
                "find the field with get_ui_tree, then set_value on its ref; or pin the app \
                 with focus_app (activate false) while it is in the background: keys are then \
                 sent to that process without raising it"
            }),
        ),
        "mouse_action" => {
            let instead = match kind.as_str() {
                "click" | "double" => Some(
                    "ui_action click on the element's ref (find it with get_ui_tree) presses it \
                     through accessibility, with no cursor",
                ),
                "right_click" if backend.route_for(SemanticAction::RightClick) == Route::Ax => {
                    Some("ui_action right_click on the element's ref")
                }
                _ => None,
            };
            Refusal::new(format!("mouse_action {kind}").trim_end(), POINTER, instead)
        }
        "scroll" => Refusal::new(
            tool,
            "scroll with the real pointer",
            Some(
                "ui_action scroll_into_view on the element you want in view, or increment / \
                 decrement on a slider or scroll bar",
            ),
        ),
        "hover" => Refusal::new(tool, POINTER, None),
        "drag_drop" => Refusal::new(
            tool,
            "press and drag with the real pointer",
            Some(
                "select_text selects a range of text without dragging; any other drag has no \
                 background-safe equivalent",
            ),
        ),
        "input_showcase" => Refusal::new(
            tool,
            "configure pointer gliding, which only shapes pointer moves, and every pointer move \
             is refused",
            Some("there is nothing to configure: no tool moves the pointer in this mode"),
        ),
        other => Refusal::new(other, "use synthetic input", None),
    }
}

fn by_args(backend: &dyn InputBackend, tool: &str, args: &Value) -> Option<Refusal> {
    match tool {
        "ui_action" => {
            let name = snake(args.get("action").and_then(Value::as_str)?);
            // An unparseable action fails with INVALID_ARGS before it does
            // anything, so it is not this layer's to refuse.
            let action = SemanticAction::parse(&name)?;
            let option = args
                .get("option")
                .and_then(Value::as_str)
                .is_some_and(|o| !o.is_empty());
            action_refusal(backend, tool, &name, action, option)
        }
        "input_steps" => {
            // Every step is judged as it would be alone, before the first one
            // runs: a refusal is static, so checking up front means a refused
            // step never lets the ones before it act. A step this module
            // would not run in a batch is not judged here; the batch's own
            // validation names it.
            args.get("steps")
                .and_then(Value::as_array)?
                .iter()
                .enumerate()
                .find_map(|(i, step)| {
                    let inner = step.get("tool").and_then(Value::as_str)?;
                    if !crate::steps::BATCHABLE.contains(&inner) {
                        return None;
                    }
                    let none = Value::Object(Default::default());
                    let r = tool_refusal(backend, inner, step.get("args").unwrap_or(&none))?;
                    Some(Refusal {
                        what: format!("input_steps step {i} ({})", r.what),
                        ..r
                    })
                })
        }
        "ui_fill_form" => {
            let fields = args.get("fields").and_then(Value::as_array)?;
            fields
                .iter()
                .chain(args.get("submit"))
                .find_map(|f| field_refusal(backend, f))
        }
        // Keys sent to a pinned background app by process touch neither the
        // frontmost app nor the pointer, so they are let through; with the app
        // in front, or nothing pinned, they would go to the foreground.
        "keyboard_type" if keys_go_to_background(backend) => None,
        "keyboard_type" => Some(always_refused(backend, tool, args)),
        "keyboard_shortcut" => {
            let hold = match args.get("action").and_then(Value::as_str) {
                Some(a) if a.eq_ignore_ascii_case("down") => true,
                // `up` only lets go; a bad `action` is INVALID_ARGS.
                Some(a) if !a.eq_ignore_ascii_case("press") => return None,
                _ => false,
            };
            if !hold && keys_go_to_background(backend) {
                return None;
            }
            Some(if hold {
                Refusal::new(
                    "keyboard_shortcut down",
                    "hold keys down, and every later synthetic pointer or key event would carry them",
                    None,
                )
            } else {
                Refusal::new(
                    "keyboard_shortcut",
                    KEYS,
                    Some(
                        "ui_action on the control the shortcut would trigger; menu_list names a \
                         menu item and its shortcut without opening anything; or pin the app \
                         with focus_app (activate false) while it is in the background and the \
                         chord is sent to that process without raising it",
                    ),
                )
            })
        }
        _ => None,
    }
}

/// Whether `tool` called with `args` takes the user's pointer, keyboard or
/// foreground, which is what `foreground = "polite"` borrows the desktop for.
///
/// It is the question `never` asks, answered the other way: a call `never`
/// would refuse is a call that needs the desktop, so the two settings cannot
/// disagree about what counts. `input_showcase` is the one exception: it only
/// shapes later pointer moves and needs the desktop for nothing.
pub(crate) fn takes_foreground(backend: &dyn InputBackend, tool: &str, args: &Value) -> bool {
    tool != "input_showcase"
        && TOOLS.iter().any(|(n, _)| *n == tool)
        && tool_refusal(backend, tool, args).is_some()
}

/// What `foreground = "polite"` cannot do at all, said plainly, or `None`.
///
/// A key held with `keyboard_shortcut` `down` outlives the call, so the desktop
/// it borrowed cannot be given back at the end of that call, and it would
/// carry into whatever the person does next. A batch is judged step by step,
/// before the first one runs.
pub(crate) fn polite_refusal(tool: &str, args: &Value) -> Option<String> {
    let holds = |a: &Value| {
        a.get("action")
            .and_then(Value::as_str)
            .is_some_and(|x| x.eq_ignore_ascii_case("down"))
    };
    let (what, hit) = match tool {
        "keyboard_shortcut" => ("keyboard_shortcut down".to_string(), holds(args)),
        "input_steps" => {
            let at = args
                .get("steps")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .position(|st| {
                    st.get("tool").and_then(Value::as_str) == Some("keyboard_shortcut")
                        && st.get("args").is_some_and(holds)
                });
            (
                format!(
                    "input_steps step {} (keyboard_shortcut down)",
                    at.unwrap_or(0)
                ),
                at.is_some(),
            )
        }
        _ => return None,
    };
    hit.then(|| {
        format!(
            "{what} was refused and nothing was done: this server runs with foreground = \
             \"polite\", which borrows the keyboard for one call and gives it back, and a key \
             held down across calls cannot be given back at the end of one. Use \
             keyboard_shortcut without action (a press), or set foreground = \"allow\""
        )
    })
}

/// What `ui_fill_form` will do with one field, as far as the foreground is
/// concerned: mirrors how the tool reads `action`, `value` and `option`.
fn field_refusal(backend: &dyn InputBackend, field: &Value) -> Option<Refusal> {
    let name = snake(field.get("action").and_then(Value::as_str)?);
    let action = SemanticAction::parse(&name)?;
    let value_is_text = field.get("value").is_some_and(Value::is_string);
    let has_option = field
        .get("option")
        .and_then(Value::as_str)
        .is_some_and(|o| !o.is_empty())
        || (action == SemanticAction::Select && value_is_text);
    action_refusal(backend, "ui_fill_form", &name, action, has_option)
}

/// Whether the accessibility action `action` may run under `never`.
///
/// `has_option` is `select` with an entry to choose, which opens the popup.
fn action_refusal(
    backend: &dyn InputBackend,
    tool: &str,
    name: &str,
    action: SemanticAction,
    has_option: bool,
) -> Option<Refusal> {
    let what = format!("{tool} {name}");
    match action {
        SemanticAction::Focus if !focus_is_quiet_on(backend.platform()) => Some(Refusal::new(
            what,
            UNVERIFIED_FOCUS,
            Some(
                "set_value and select_text act on a text field without focusing it; \
                 ui_action click presses a control",
            ),
        )),
        SemanticAction::ShowMenu => Some(Refusal::new(
            what,
            UNVERIFIED_MENU,
            Some(
                "menu_list reads a menu's items without opening it; ui_action click on the \
                 control that does the same thing",
            ),
        )),
        SemanticAction::Select if has_option => Some(Refusal::new(
            what,
            UNVERIFIED_MENU,
            Some(
                "set_value on a combo box takes the entry's text; ui_action click on a list \
                 row or radio button picks it",
            ),
        )),
        a if backend.route_for(a) == Route::Hid => Some(Refusal::new(
            what,
            "click with the real pointer: on this platform the action has no accessibility route",
            None,
        )),
        _ => None,
    }
}

/// The refusal for a backend method that is synthetic input by nature.
fn method_refusal(method: &str, instead: Option<&str>) -> InputError {
    let effect = match method {
        "type_text" | "key_combo" | "key_down" => KEYS,
        "scroll_at" => "scroll with the real pointer",
        "drag" => "press and drag with the real pointer",
        "choose_option" => UNVERIFIED_MENU,
        _ => POINTER,
    };
    InputError::Refused(Refusal::new(method, effect, instead).one_line())
}

/// An [`InputBackend`] that posts no synthetic input and activates nothing,
/// whatever it is asked: the backend boundary of `foreground = "never"`.
///
/// Only the composition root builds one, and only when the operator set
/// `never`; with `allow` the real backend is used as it always was. It
/// forwards what is background-safe (reads, `set_value`, `select_range`, the
/// clipboard, the sensors the human-override watcher reads) and refuses the
/// rest with [`InputError::Refused`] before the inner backend is touched.
pub struct ForegroundInput {
    inner: Arc<dyn InputBackend>,
}

impl ForegroundInput {
    pub fn new(inner: Arc<dyn InputBackend>) -> Self {
        ForegroundInput { inner }
    }

    /// The per-process key route, where it has been measured to leave the
    /// foreground alone and only to a pinned app that is not frontmost: the
    /// check that keeps a tool added later from sending keys by process to
    /// the app the person is using.
    fn require_pid_route(&self) -> Result<(), InputError> {
        if keys_go_to_background(&*self.inner) {
            Ok(())
        } else {
            Err(method_refusal("type_text_to_pid", Some("set_value")))
        }
    }
}

#[async_trait]
impl InputBackend for ForegroundInput {
    async fn perform(
        &self,
        node_id: u64,
        action: SemanticAction,
        option: Option<&str>,
    ) -> Result<Route, InputError> {
        let label = format!("{action:?}");
        if let Some(r) = action_refusal(&*self.inner, "perform", &label, action, false) {
            return Err(InputError::Refused(r.one_line()));
        }
        self.inner.perform(node_id, action, option).await
    }
    fn route_for(&self, action: SemanticAction) -> Route {
        self.inner.route_for(action)
    }
    async fn choose_option(&self, _node_id: u64, _option: &str) -> Result<Choice, InputError> {
        Err(method_refusal(
            "choose_option",
            Some("set_value on a combo box takes the entry's text"),
        ))
    }
    async fn read_element(&self, node_id: u64) -> Result<Reading, InputError> {
        self.inner.read_element(node_id).await
    }
    async fn set_value(&self, node_id: u64, text: &str) -> Result<(), InputError> {
        self.inner.set_value(node_id, text).await
    }
    async fn selection(&self, node_id: Option<u64>) -> Result<Selection, InputError> {
        self.inner.selection(node_id).await
    }
    async fn select_range(
        &self,
        node_id: u64,
        start: usize,
        end: usize,
    ) -> Result<Selection, InputError> {
        self.inner.select_range(node_id, start, end).await
    }
    async fn type_text(&self, _text: &str) -> Result<(), InputError> {
        Err(method_refusal("type_text", Some("set_value")))
    }
    fn background_key_target(&self) -> Option<crate::BackgroundTarget> {
        if pid_route_is_quiet_on(self.inner.platform()) {
            self.inner.background_key_target()
        } else {
            None
        }
    }
    async fn type_text_to_pid(&self, pid: i32, text: &str) -> Result<(), InputError> {
        self.require_pid_route()?;
        self.inner.type_text_to_pid(pid, text).await
    }
    async fn key_combo_to_pid(&self, pid: i32, combo: &str) -> Result<(), InputError> {
        self.require_pid_route()?;
        self.inner.key_combo_to_pid(pid, combo).await
    }
    async fn read_focused_text(&self, pid: i32) -> Result<crate::FocusedText, InputError> {
        self.inner.read_focused_text(pid).await
    }
    async fn key_combo(&self, _combo: &str) -> Result<(), InputError> {
        Err(method_refusal("key_combo", Some("ui_action")))
    }
    async fn mouse(
        &self,
        _kind: MouseKind,
        _x: f64,
        _y: f64,
        _button: Option<&str>,
        _modifiers: &[String],
    ) -> Result<(), InputError> {
        Err(method_refusal("mouse", Some("ui_action click")))
    }
    async fn scroll_at(
        &self,
        _x: f64,
        _y: f64,
        _dir: ScrollDir,
        _amount: i32,
        _modifiers: &[String],
    ) -> Result<(), InputError> {
        Err(method_refusal(
            "scroll_at",
            Some("ui_action scroll_into_view"),
        ))
    }
    async fn hover(&self, _x: f64, _y: f64) -> Result<(), InputError> {
        Err(method_refusal("hover", None))
    }
    async fn drag(
        &self,
        _from: (f64, f64),
        _route: &[(f64, f64)],
        _modifiers: &[String],
        _hold_ms: u64,
        _since_takeover: u64,
    ) -> Result<(), InputError> {
        Err(method_refusal("drag", None))
    }
    async fn key_down(&self, _key: &str) -> Result<(), InputError> {
        Err(method_refusal("key_down", None))
    }
    /// Succeeds without forwarding. A release must never fail (a vetoed
    /// release is a stuck key), but `key_down` is refused, so nothing is down
    /// through this wrapper to release; forwarding would post a key-up event
    /// for a key that was never pressed, which on Linux reaches the portal.
    async fn key_up(&self, _key: &str) -> Result<(), InputError> {
        Ok(())
    }
    async fn clipboard_read(&self, format: ClipFormat) -> Result<ClipData, InputError> {
        self.inner.clipboard_read(format).await
    }
    fn input_target(&self) -> Option<String> {
        self.inner.input_target()
    }
    async fn clipboard_write(&self, format: ClipFormat, data: &str) -> Result<(), InputError> {
        self.inner.clipboard_write(format, data).await
    }
    fn platform(&self) -> &'static str {
        self.inner.platform()
    }
    async fn pointer_position(&self) -> Result<Option<(f64, f64)>, InputError> {
        self.inner.pointer_position().await
    }
    fn pointer_unavailable_reason(&self) -> Option<String> {
        self.inner.pointer_unavailable_reason()
    }
    fn recent_pointer_sets(&self) -> Vec<crate::SetPoint> {
        self.inner.recent_pointer_sets()
    }
    fn cancel_pending(&self) {
        self.inner.cancel_pending()
    }
    fn takeover_generation(&self) -> u64 {
        self.inner.takeover_generation()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn focus_is_let_through_only_where_it_was_measured() {
        assert!(focus_is_quiet_on("macos"));
        for other in ["linux", "windows", "mock", ""] {
            assert!(!focus_is_quiet_on(other), "{other:?}");
        }
    }

    #[test]
    fn a_held_key_is_refused_under_polite_alone_and_inside_a_batch() {
        let down = json!({ "combo": "shift", "action": "down" });
        let msg = polite_refusal("keyboard_shortcut", &down).expect("refused");
        assert!(msg.contains("polite") && msg.contains("nothing was done"));
        assert!(polite_refusal("keyboard_shortcut", &json!({ "combo": "a" })).is_none());
        assert!(polite_refusal("keyboard_shortcut", &json!({ "action": "up" })).is_none());
        let batch = json!({ "steps": [
            { "tool": "keyboard_type", "args": { "text": "x" } },
            { "tool": "keyboard_shortcut", "args": down },
        ]});
        let msg = polite_refusal("input_steps", &batch).expect("refused");
        assert!(msg.contains("step 1"), "{msg}");
        assert!(polite_refusal("input_steps", &json!({ "steps": [] })).is_none());
        assert!(polite_refusal("mouse_action", &json!({ "action": "down" })).is_none());
    }

    #[test]
    fn per_process_keys_are_let_through_only_where_they_were_measured() {
        assert!(pid_route_is_quiet_on("macos"));
        for other in ["linux", "windows", "mock", ""] {
            assert!(!pid_route_is_quiet_on(other), "{other:?}");
        }
    }
}
