//! A locked screen, as the input engine sees it: which calls would post
//! synthetic input, and the backend wrapper that holds the line for the ones
//! the tool layer cannot see coming.
//!
//! While the session is locked or switched away, the system drops key and
//! pointer events a program posts, and the post reports success. Two layers,
//! for the reason `foreground` has two: the tool layer ([`posts_input`], used
//! first in `InputModule::call`) refuses before anything runs, so a batch or a
//! form does not half-finish, and [`ScreenGuardInput`] is the boundary every
//! posting method sits behind, so a path added later cannot report a
//! discarded event as done.

use std::sync::Arc;

use async_trait::async_trait;
use mcp_types::{screen_refusal, ScreenState};
use serde_json::Value;

use crate::backend::{
    Choice, ClipData, ClipFormat, InputBackend, InputError, MouseKind, Reading, Route, ScrollDir,
    Selection, SemanticAction,
};

fn snake(s: &str) -> String {
    s.trim().to_lowercase().replace(['-', ' '], "_")
}

/// Whether `tool` called with `args` would post key or pointer events, by
/// any route (the system stream or one process's queue).
///
/// What it leaves out is on purpose: accessibility actions (a route of `ax`
/// posts nothing), reads, the clipboard, and `keyboard_shortcut` `up`, which
/// only lets go of a key. A release must not be refused (a key the ledger
/// still counts as held would stay held), and while the session is locked
/// the system has already let go of everything.
pub(crate) fn posts_input(backend: &dyn InputBackend, tool: &str, args: &Value) -> bool {
    match tool {
        "mouse_action" | "scroll" | "hover" | "drag_drop" | "keyboard_type" => true,
        "keyboard_shortcut" => !args
            .get("action")
            .and_then(Value::as_str)
            .is_some_and(|a| a.eq_ignore_ascii_case("up")),
        "ui_action" => args
            .get("action")
            .and_then(Value::as_str)
            .and_then(|a| SemanticAction::parse(&snake(a)))
            .is_some_and(|a| backend.route_for(a) == Route::Hid),
        "ui_fill_form" => args
            .get("fields")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .chain(args.get("submit"))
            .filter_map(|f| f.get("action").and_then(Value::as_str))
            .filter_map(|a| SemanticAction::parse(&snake(a)))
            .any(|a| backend.route_for(a) == Route::Hid),
        "input_steps" => args
            .get("steps")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|st| {
                let none = Value::Object(Default::default());
                st.get("tool")
                    .and_then(Value::as_str)
                    .is_some_and(|t| posts_input(backend, t, st.get("args").unwrap_or(&none)))
            }),
        _ => false,
    }
}

/// The refusal for a call, as an envelope, or `None` when the session can
/// take it (or nothing is posted).
pub(crate) fn tool_screen_refusal(
    backend: &dyn InputBackend,
    tool: &str,
    args: &Value,
) -> Option<mcp_types::Envelope> {
    if !posts_input(backend, tool, args) {
        return None;
    }
    // Read after the cheap argument check, so a call that posts nothing never
    // asks the system anything.
    screen_refusal(backend.screen_state(), tool).map(|r| r.envelope(tool))
}

/// An [`InputBackend`] that posts nothing while the session cannot receive
/// it: the backend boundary of the locked-screen check. Always wrapped in by
/// the composition root, outside any `foreground` wrapper, and forwarding
/// everything that is not a post.
pub struct ScreenGuardInput {
    inner: Arc<dyn InputBackend>,
}

impl ScreenGuardInput {
    pub fn new(inner: Arc<dyn InputBackend>) -> Self {
        ScreenGuardInput { inner }
    }

    fn check(&self, what: &str) -> Result<(), InputError> {
        match screen_refusal(self.inner.screen_state(), what) {
            Some(r) => Err(InputError::Failed(r.one_line())),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl InputBackend for ScreenGuardInput {
    async fn perform(
        &self,
        node_id: u64,
        action: SemanticAction,
        option: Option<&str>,
    ) -> Result<Route, InputError> {
        if self.inner.route_for(action) == Route::Hid {
            self.check("perform")?;
        }
        self.inner.perform(node_id, action, option).await
    }
    fn route_for(&self, action: SemanticAction) -> Route {
        self.inner.route_for(action)
    }
    async fn choose_option(&self, node_id: u64, option: &str) -> Result<Choice, InputError> {
        self.inner.choose_option(node_id, option).await
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
    fn background_key_target(&self) -> Option<crate::BackgroundTarget> {
        self.inner.background_key_target()
    }
    async fn type_text_to_pid(&self, pid: i32, text: &str) -> Result<(), InputError> {
        self.check("type_text_to_pid")?;
        self.inner.type_text_to_pid(pid, text).await
    }
    async fn key_combo_to_pid(&self, pid: i32, combo: &str) -> Result<(), InputError> {
        self.check("key_combo_to_pid")?;
        self.inner.key_combo_to_pid(pid, combo).await
    }
    async fn read_focused_text(&self, pid: i32) -> Result<crate::FocusedText, InputError> {
        self.inner.read_focused_text(pid).await
    }
    async fn user_idle_ms(&self) -> Option<u64> {
        self.inner.user_idle_ms().await
    }
    async fn desk_state(&self) -> crate::DeskState {
        self.inner.desk_state().await
    }
    fn restore_desk(&self, saved: &crate::DeskState, pointer: bool) {
        self.inner.restore_desk(saved, pointer)
    }
    async fn type_text(&self, text: &str) -> Result<(), InputError> {
        self.check("type_text")?;
        self.inner.type_text(text).await
    }
    async fn key_combo(&self, combo: &str) -> Result<(), InputError> {
        self.check("key_combo")?;
        self.inner.key_combo(combo).await
    }
    async fn mouse(
        &self,
        kind: MouseKind,
        x: f64,
        y: f64,
        button: Option<&str>,
        modifiers: &[String],
    ) -> Result<(), InputError> {
        self.check("mouse")?;
        self.inner.mouse(kind, x, y, button, modifiers).await
    }
    async fn scroll_at(
        &self,
        x: f64,
        y: f64,
        dir: ScrollDir,
        amount: i32,
        modifiers: &[String],
    ) -> Result<(), InputError> {
        self.check("scroll_at")?;
        self.inner.scroll_at(x, y, dir, amount, modifiers).await
    }
    async fn scroll_pixels(
        &self,
        x: f64,
        y: f64,
        event: &crate::trackpad::PixelScroll,
        modifiers: &[String],
    ) -> Result<(), InputError> {
        self.check("scroll_pixels")?;
        self.inner.scroll_pixels(x, y, event, modifiers).await
    }
    fn pixel_momentum(&self) -> bool {
        self.inner.pixel_momentum()
    }
    async fn hover(&self, x: f64, y: f64) -> Result<(), InputError> {
        self.check("hover")?;
        self.inner.hover(x, y).await
    }
    async fn drag(
        &self,
        from: (f64, f64),
        route: &[(f64, f64)],
        modifiers: &[String],
        hold_ms: u64,
        since_takeover: u64,
    ) -> Result<(), InputError> {
        self.check("drag")?;
        self.inner
            .drag(from, route, modifiers, hold_ms, since_takeover)
            .await
    }
    async fn key_down(&self, key: &str) -> Result<(), InputError> {
        self.check("key_down")?;
        self.inner.key_down(key).await
    }
    /// Forwarded: a release must never fail, and `key_down` is refused while
    /// the session is blocked, so nothing is down because of this wrapper.
    async fn key_up(&self, key: &str) -> Result<(), InputError> {
        self.inner.key_up(key).await
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
    fn screen_state(&self) -> ScreenState {
        self.inner.screen_state()
    }
}
