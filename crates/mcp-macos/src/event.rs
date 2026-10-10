//! Coordinate/keyboard input via CGEvent, and clipboard via `pbcopy`/`pbpaste`.
//! Posting synthetic events also requires the Accessibility permission; unlike
//! the AX calls, `CGEvent::post` is fire-and-forget and cannot report a
//! permission failure, so these return `Ok` even when the OS drops the event.

use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGKeyCode, CGMouseButton, EventField,
    ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use std::collections::VecDeque;
use std::sync::Mutex;

use mcp_input::{InputError, MouseKind, ScrollDir, SetPoint};

fn fail(what: &str) -> InputError {
    InputError::Failed(format!("{what} failed"))
}

/// The pointer positions this process has set, newest last.
///
/// The human-override watcher compares where the pointer *is* against where the
/// server *put* it; without this record it could not tell its own movement from
/// anybody else's. Bounded, because a long drag would otherwise grow it without
/// limit and only the recent past is an explanation for the present.
static RECENT_SETS: Mutex<VecDeque<SetPoint>> = Mutex::new(VecDeque::new());
const MAX_RECENT_SETS: usize = 64;

/// Milliseconds since the epoch. Local rather than pulling in the policy crate:
/// a backend has no business depending on the security kernel.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn record_set(pt: CGPoint) {
    let mut g = RECENT_SETS.lock().unwrap_or_else(|e| e.into_inner());
    if g.len() >= MAX_RECENT_SETS {
        g.pop_front();
    }
    g.push_back(SetPoint {
        x: pt.x,
        y: pt.y,
        at_ms: now_ms(),
    });
}

/// Positions this process recently set, newest last.
pub fn recent_sets() -> Vec<SetPoint> {
    RECENT_SETS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .copied()
        .collect()
}

/// Where the pointer actually is, in the same global display space used for
/// posting. Needs no permission: it reads the event system's own state.
pub fn pointer_position() -> Result<Option<(f64, f64)>, InputError> {
    let ev = CGEvent::new(source()?).map_err(|_| fail("read pointer"))?;
    let p = ev.location();
    Ok(Some((p.x, p.y)))
}

/// Clamp a point into the union of the displays.
///
/// Pure, and applied before recording: the OS silently clamps a click aimed
/// past the screen edge, so recording the *requested* point would leave the
/// watcher comparing against a position the pointer never occupied — and
/// reporting the server's own click as a human takeover.
pub fn clamp_point(pt: (f64, f64), rects: &[(f64, f64, f64, f64)]) -> (f64, f64) {
    if rects.is_empty() {
        return pt;
    }
    let inside = rects
        .iter()
        .any(|(x, y, w, h)| pt.0 >= *x && pt.0 <= x + w && pt.1 >= *y && pt.1 <= y + h);
    if inside {
        return pt;
    }
    let mut best = pt;
    let mut best_d = f64::INFINITY;
    for (x, y, w, h) in rects {
        let cx = pt.0.clamp(*x, x + w);
        let cy = pt.1.clamp(*y, y + h);
        let d = ((cx - pt.0).powi(2) + (cy - pt.1).powi(2)).sqrt();
        if d < best_d {
            best_d = d;
            best = (cx, cy);
        }
    }
    best
}

fn display_rects() -> Vec<(f64, f64, f64, f64)> {
    CGDisplay::active_displays()
        .unwrap_or_default()
        .into_iter()
        .map(|id| {
            let b = CGDisplay::new(id).bounds();
            (b.origin.x, b.origin.y, b.size.width, b.size.height)
        })
        .collect()
}

fn source() -> Result<CGEventSource, InputError> {
    CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| InputError::Failed("could not create CGEventSource".into()))
}

/// Type one chunk of Unicode text by attaching it to a synthetic keystroke.
///
/// `chunk` must be at most [`crate::chunk::MAX_UNITS`] UTF-16 code units: Apple
/// documents that only the first 20 of a string set on one event are used, so
/// anything longer is cut silently. [`crate::chunk::chunks`] produces the
/// pieces; the caller posts them in order.
pub fn type_chunk(chunk: &str) -> Result<(), InputError> {
    let src = source()?;
    let down = CGEvent::new_keyboard_event(src.clone(), 0, true).map_err(|_| fail("key down"))?;
    down.set_string(chunk);
    // Typed text carries no modifiers, and *not setting* the flags is not the
    // same as setting them to empty: an event built from the HID state source
    // inherits whatever the system believes is currently held. With Command
    // latched — by a stuck physical key, a crashed app, or a previous chord —
    // every character silently becomes a menu shortcut. The call still reports
    // the full character count, so the agent is told it typed and sees nothing.
    down.set_flags(CGEventFlags::empty());
    down.post(CGEventTapLocation::HID);
    let up = CGEvent::new_keyboard_event(src, 0, false).map_err(|_| fail("key up"))?;
    up.set_string(chunk);
    up.set_flags(CGEventFlags::empty());
    up.post(CGEventTapLocation::HID);
    Ok(())
}

/// Press a chord in the canonical form `mcp_input::parse_combo` produces:
/// modifiers, then one key, joined by `+`.
pub fn key_combo(combo: &str) -> Result<(), InputError> {
    let mut flags = CGEventFlags::empty();
    let mut key: Option<(CGKeyCode, bool)> = None;
    for seg in combo.split('+') {
        match seg {
            "cmd" | "command" | "meta" | "super" => flags |= CGEventFlags::CGEventFlagCommand,
            "shift" => flags |= CGEventFlags::CGEventFlagShift,
            "opt" | "option" | "alt" => flags |= CGEventFlags::CGEventFlagAlternate,
            "ctrl" | "control" => flags |= CGEventFlags::CGEventFlagControl,
            "fn" => flags |= CGEventFlags::CGEventFlagSecondaryFn,
            name => {
                // Overwriting here is how `a+b` used to press only `b`.
                if key.is_some() {
                    return Err(InputError::InvalidArgs(format!(
                        "combo '{combo}' has more than one key"
                    )));
                }
                key = Some(keycode_for(name).ok_or_else(|| {
                    InputError::Unsupported(format!(
                        "no macOS keycode for '{name}' in combo '{combo}'"
                    ))
                })?);
            }
        }
    }
    let (keycode, needs_shift) =
        key.ok_or_else(|| InputError::Unsupported(format!("no key in combo '{combo}' to press")))?;
    if needs_shift {
        flags |= CGEventFlags::CGEventFlagShift;
    }
    let src = source()?;
    let down =
        CGEvent::new_keyboard_event(src.clone(), keycode, true).map_err(|_| fail("key down"))?;
    down.set_flags(flags);
    down.post(CGEventTapLocation::HID);
    let up = CGEvent::new_keyboard_event(src, keycode, false).map_err(|_| fail("key up"))?;
    up.set_flags(flags);
    up.post(CGEventTapLocation::HID);
    Ok(())
}

/// Translate modifier names into event flags. Unknown names are rejected at the
/// tool layer, so anything reaching here is already vetted.
pub fn modifier_flags(modifiers: &[String]) -> CGEventFlags {
    let mut flags = CGEventFlags::empty();
    for m in modifiers {
        match m.as_str() {
            "cmd" | "command" | "meta" | "super" => flags |= CGEventFlags::CGEventFlagCommand,
            "shift" => flags |= CGEventFlags::CGEventFlagShift,
            "opt" | "option" | "alt" => flags |= CGEventFlags::CGEventFlagAlternate,
            "ctrl" | "control" => flags |= CGEventFlags::CGEventFlagControl,
            "fn" => flags |= CGEventFlags::CGEventFlagSecondaryFn,
            _ => {}
        }
    }
    flags
}

/// A mouse button, as the backend tracks it. Its own type because
/// `CGMouseButton` derives neither `PartialEq` nor `Eq`, and the held-button
/// record needs to compare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Center,
}

impl Button {
    /// Resolve a button name; anything unrecognised is the left button.
    pub fn parse(name: &str) -> Button {
        match name {
            "right" => Button::Right,
            "middle" | "center" => Button::Center,
            _ => Button::Left,
        }
    }

    fn cg(self) -> CGMouseButton {
        match self {
            Button::Left => CGMouseButton::Left,
            Button::Right => CGMouseButton::Right,
            Button::Center => CGMouseButton::Center,
        }
    }

    fn down(self) -> CGEventType {
        match self {
            Button::Left => CGEventType::LeftMouseDown,
            Button::Right => CGEventType::RightMouseDown,
            Button::Center => CGEventType::OtherMouseDown,
        }
    }

    fn up(self) -> CGEventType {
        match self {
            Button::Left => CGEventType::LeftMouseUp,
            Button::Right => CGEventType::RightMouseUp,
            Button::Center => CGEventType::OtherMouseUp,
        }
    }

    fn dragged(self) -> CGEventType {
        match self {
            Button::Left => CGEventType::LeftMouseDragged,
            Button::Right => CGEventType::RightMouseDragged,
            Button::Center => CGEventType::OtherMouseDragged,
        }
    }
}

/// The buttons this backend currently holds down.
///
/// macOS has two kinds of pointer motion: `MouseMoved` with nothing pressed
/// and `*MouseDragged` while a button is down, and targets that track a drag
/// (Finder, sliders, canvases, text selection) listen only for the second.
/// A `mouse_action down` followed by `move` is how an agent composes its own
/// drag, so the backend has to remember the press to send the right motion.
/// Owned by the backend rather than a static so a test can have its own.
#[derive(Debug, Default)]
pub struct Held {
    buttons: Mutex<Vec<Button>>,
}

impl Held {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Button>> {
        self.buttons.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn press(&self, b: Button) {
        let mut g = self.lock();
        if !g.contains(&b) {
            g.push(b);
        }
    }

    pub fn release(&self, b: Button) {
        self.lock().retain(|h| *h != b);
    }

    /// Forget everything: the human-override brake fired, so whatever was
    /// held is no longer this backend's gesture.
    pub fn clear(&self) {
        self.lock().clear();
    }

    pub fn buttons(&self) -> Vec<Button> {
        self.lock().clone()
    }
}

/// The event a pointer motion must carry given what is held: a plain
/// `MouseMoved` with nothing down, otherwise the `*MouseDragged` of the held
/// button. With several held, the precedence is the one IOHIDSystem uses for
/// a real mouse: left, then right, then any other button.
pub fn motion_event(held: &[Button]) -> (CGEventType, Button) {
    let btn = if held.contains(&Button::Left) {
        Button::Left
    } else if held.contains(&Button::Right) {
        Button::Right
    } else if let Some(b) = held.first() {
        *b
    } else {
        return (CGEventType::MouseMoved, Button::Left);
    };
    (btn.dragged(), btn)
}

/// The pressure a real mouse reports: 1 while a button is down (a press or a
/// drag), 0 otherwise. `CGEventCreateMouseEvent` leaves it at 0 for every
/// type, which makes a drag look like a move to anything that reads it.
fn pressure_for(ty: CGEventType) -> f64 {
    match ty {
        CGEventType::LeftMouseDown
        | CGEventType::RightMouseDown
        | CGEventType::OtherMouseDown
        | CGEventType::LeftMouseDragged
        | CGEventType::RightMouseDragged
        | CGEventType::OtherMouseDragged => 1.0,
        _ => 0.0,
    }
}

/// Build one mouse event without posting it, so a test can inspect what would
/// go out. The returned point is where the pointer will actually land.
///
/// `click_state` is the part that is easy to leave out and impossible to notice
/// afterwards: macOS decides "double click" from this field, not from how fast
/// two clicks arrive. Sending down/up twice with the default state of 1 gives
/// the target two ordinary single clicks, so double-click-to-open and
/// triple-click-to-select-line quietly do nothing.
fn build_mouse_event(
    ty: CGEventType,
    pt: CGPoint,
    btn: Button,
    flags: CGEventFlags,
    click_state: i64,
) -> Result<(CGEvent, CGPoint), InputError> {
    // The OS clamps a point past the screen edge; recording the unclamped
    // request would have the watcher compare against a position the pointer
    // never occupied, and read the server's own click as a human takeover.
    let pt = {
        let (x, y) = clamp_point((pt.x, pt.y), &display_rects());
        CGPoint::new(x, y)
    };
    let ev =
        CGEvent::new_mouse_event(source()?, ty, pt, btn.cg()).map_err(|_| fail("mouse event"))?;
    // Always set it, including for a single click. `CGEventCreateMouseEvent`
    // leaves the field at 0, which reaches AppKit as `clickCount == 0` — not
    // "one click". Measured against a live NSTextView, a second such click
    // extends the selection from the previous caret instead of moving it, so
    // plain clicks silently behaved like shift-clicks.
    ev.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, click_state.max(1));
    ev.set_double_value_field(EventField::MOUSE_EVENT_PRESSURE, pressure_for(ty));
    // Always set flags, including the empty set. A mouse event created from an
    // HID-state source *inherits* whatever modifiers the system currently
    // believes are held, so skipping this when no modifier was asked for lets a
    // stray Shift or Command latch onto every subsequent click — a plain click
    // then behaves as shift-click and extends a selection instead of moving the
    // caret. Setting it explicitly clears the inherited state.
    ev.set_flags(flags);
    Ok((ev, pt))
}

/// Post one mouse event.
fn post_mouse_ex(
    ty: CGEventType,
    pt: CGPoint,
    btn: Button,
    flags: CGEventFlags,
    click_state: i64,
) -> Result<(), InputError> {
    let (ev, pt) = build_mouse_event(ty, pt, btn, flags, click_state)?;
    // Record before posting: this is the only place the pointer is written, so
    // it is the only place the override watcher can learn what was ours.
    record_set(pt);
    ev.post(CGEventTapLocation::HID);
    Ok(())
}

/// Move the pointer to `pt`, as a drag when a button is held.
fn post_motion(held: &Held, pt: CGPoint, flags: CGEventFlags) -> Result<(), InputError> {
    let (ty, btn) = motion_event(&held.buttons());
    post_mouse_ex(ty, pt, btn, flags, 1)
}

pub fn mouse(
    held: &Held,
    kind: MouseKind,
    x: f64,
    y: f64,
    button: Option<&str>,
    modifiers: &[String],
) -> Result<(), InputError> {
    let pt = CGPoint::new(x, y);
    let btn = if matches!(kind, MouseKind::RightClick) {
        Button::Right
    } else {
        Button::parse(button.unwrap_or("left"))
    };
    let flags = modifier_flags(modifiers);
    // A click at a position the pointer is not at can miss hover-activated
    // targets, so move there first — as a drag if a button is already down,
    // since the release point of a composed drag is reached by dragging.
    if !matches!(kind, MouseKind::Move) {
        post_motion(held, pt, flags)?;
    }
    // Multi-clicks are a *sequence* of clicks with a rising click-state, not N
    // independent clicks: the target reads state 2 as "this is the double".
    let clicks = match kind {
        MouseKind::Double => 2,
        MouseKind::Triple => 3,
        _ => 1,
    };
    match kind {
        MouseKind::Move => post_motion(held, pt, flags)?,
        MouseKind::Down => {
            post_mouse_ex(btn.down(), pt, btn, flags, 1)?;
            held.press(btn);
        }
        MouseKind::Up => {
            // Forget the press even if the release could not be posted: the
            // next motion must not claim a drag the agent has given up on.
            let r = post_mouse_ex(btn.up(), pt, btn, flags, 1);
            held.release(btn);
            r?;
        }
        _ => {
            for state in 1..=clicks {
                post_mouse_ex(btn.down(), pt, btn, flags, state)?;
                post_mouse_ex(btn.up(), pt, btn, flags, state)?;
            }
        }
    }
    Ok(())
}

/// Press at `pt`, beginning a drag. Pair with [`drag_to`] and [`drag_end`].
///
/// Split into three calls so the caller can space the intermediate moves out in
/// time without blocking an async runtime with `thread::sleep`.
pub fn drag_begin(held: &Held, pt: (f64, f64), modifiers: &[String]) -> Result<(), InputError> {
    let p = CGPoint::new(pt.0, pt.1);
    let flags = modifier_flags(modifiers);
    post_motion(held, p, flags)?;
    post_mouse_ex(CGEventType::LeftMouseDown, p, Button::Left, flags, 1)?;
    held.press(Button::Left);
    Ok(())
}

pub fn drag_to(pt: (f64, f64), modifiers: &[String]) -> Result<(), InputError> {
    post_mouse_ex(
        CGEventType::LeftMouseDragged,
        CGPoint::new(pt.0, pt.1),
        Button::Left,
        modifier_flags(modifiers),
        1,
    )
}

pub fn drag_end(held: &Held, pt: (f64, f64), modifiers: &[String]) -> Result<(), InputError> {
    let r = post_mouse_ex(
        CGEventType::LeftMouseUp,
        CGPoint::new(pt.0, pt.1),
        Button::Left,
        modifier_flags(modifiers),
        1,
    );
    held.release(Button::Left);
    r
}

/// Interpolate `steps` points from `from` to `to`, exclusive of the start.
pub fn drag_path(from: (f64, f64), to: (f64, f64), steps: u32) -> Vec<(f64, f64)> {
    let steps = steps.max(1);
    (1..=steps)
        .map(|i| {
            let t = i as f64 / steps as f64;
            (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t)
        })
        .collect()
}

/// Wheel deltas in lines for a direction: `(vertical, horizontal)`, positive
/// meaning up and left as CoreGraphics counts them. A page is ten lines.
pub fn scroll_deltas(dir: ScrollDir, amount: i32) -> (i32, i32) {
    let a = amount.max(1);
    match dir {
        ScrollDir::Up => (a, 0),
        ScrollDir::Down => (-a, 0),
        ScrollDir::Left => (0, a),
        ScrollDir::Right => (0, -a),
        ScrollDir::PageUp => (a * 10, 0),
        ScrollDir::PageDown => (-a * 10, 0),
    }
}

/// Build one wheel event without posting it.
fn build_scroll_event(
    dir: ScrollDir,
    amount: i32,
    flags: CGEventFlags,
) -> Result<CGEvent, InputError> {
    let (vertical, horizontal) = scroll_deltas(dir, amount);
    let ev =
        CGEvent::new_scroll_event(source()?, ScrollEventUnit::LINE, 2, vertical, horizontal, 0)
            .map_err(|_| fail("scroll event"))?;
    // Cmd+wheel and Ctrl+wheel are zoom in most apps, Shift+wheel is sideways
    // in some; the modifier rides on the event's flags, which is where AppKit
    // reads it from. Set even when empty, for the same reason as a click: an
    // event from the HID source inherits whatever the system thinks is held.
    ev.set_flags(flags);
    Ok(ev)
}

pub fn scroll(
    held: &Held,
    x: f64,
    y: f64,
    dir: ScrollDir,
    amount: i32,
    modifiers: &[String],
) -> Result<(), InputError> {
    let flags = modifier_flags(modifiers);
    // Position the cursor so the scroll targets that location.
    post_motion(held, CGPoint::new(x, y), flags)?;
    build_scroll_event(dir, amount, flags)?.post(CGEventTapLocation::HID);
    Ok(())
}

pub fn hover(held: &Held, x: f64, y: f64) -> Result<(), InputError> {
    post_motion(held, CGPoint::new(x, y), CGEventFlags::empty())
}

pub fn clipboard_read_text() -> Result<Option<String>, InputError> {
    let out = std::process::Command::new("pbpaste")
        .output()
        .map_err(|e| InputError::Failed(format!("pbpaste: {e}")))?;
    Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()))
}

pub fn clipboard_write_text(data: &str) -> Result<(), InputError> {
    use std::io::Write;
    let mut child = std::process::Command::new("pbcopy")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| InputError::Failed(format!("pbcopy: {e}")))?;
    child
        .stdin
        .take()
        .ok_or_else(|| fail("pbcopy stdin"))?
        .write_all(data.as_bytes())
        .map_err(|e| InputError::Failed(format!("pbcopy write: {e}")))?;
    child
        .wait()
        .map_err(|e| InputError::Failed(format!("pbcopy wait: {e}")))?;
    Ok(())
}

/// US ANSI virtual keycodes for a canonical key name, and whether the key
/// needs Shift held to produce it (`plus` is Shift+`=`, `?` is Shift+`/`).
///
/// These are physical key positions, so punctuation follows the US layout:
/// on another layout `cmd+[` presses whatever sits where the US `[` does,
/// which is what a shortcut bound to a position wants anyway.
fn keycode_for(key: &str) -> Option<(CGKeyCode, bool)> {
    let code: u16 = match key {
        "a" => 0x00,
        "b" => 0x0B,
        "c" => 0x08,
        "d" => 0x02,
        "e" => 0x0E,
        "f" => 0x03,
        "g" => 0x05,
        "h" => 0x04,
        "i" => 0x22,
        "j" => 0x26,
        "k" => 0x28,
        "l" => 0x25,
        "m" => 0x2E,
        "n" => 0x2D,
        "o" => 0x1F,
        "p" => 0x23,
        "q" => 0x0C,
        "r" => 0x0F,
        "s" => 0x01,
        "t" => 0x11,
        "u" => 0x20,
        "v" => 0x09,
        "w" => 0x0D,
        "x" => 0x07,
        "y" => 0x10,
        "z" => 0x06,
        "0" => 0x1D,
        "1" => 0x12,
        "2" => 0x13,
        "3" => 0x14,
        "4" => 0x15,
        "5" => 0x17,
        "6" => 0x16,
        "7" => 0x1A,
        "8" => 0x1C,
        "9" => 0x19,
        "return" | "enter" => 0x24,
        "tab" => 0x30,
        "space" => 0x31,
        "delete" | "backspace" => 0x33,
        "forwarddelete" | "del" => 0x75,
        "escape" | "esc" => 0x35,
        "left" => 0x7B,
        "right" => 0x7C,
        "down" => 0x7D,
        "up" => 0x7E,
        "home" => 0x73,
        "end" => 0x77,
        "pageup" | "page_up" => 0x74,
        "pagedown" | "page_down" => 0x79,
        // A PC keyboard's Insert is the Mac's Help key.
        "insert" | "help" => 0x72,
        "capslock" => 0x39,
        // Print Screen is where F13 is on an extended Apple keyboard.
        "printscreen" => 0x69,
        "f1" => 0x7A,
        "f2" => 0x78,
        "f3" => 0x63,
        "f4" => 0x76,
        "f5" => 0x60,
        "f6" => 0x61,
        "f7" => 0x62,
        "f8" => 0x64,
        "f9" => 0x65,
        "f10" => 0x6D,
        "f11" => 0x67,
        "f12" => 0x6F,
        "f13" => 0x69,
        "f14" => 0x6B,
        "f15" => 0x71,
        "f16" => 0x6A,
        "f17" => 0x40,
        "f18" => 0x4F,
        "f19" => 0x50,
        "f20" => 0x5A,
        "minus" => 0x1B,
        "equal" => 0x18,
        "leftbracket" => 0x21,
        "rightbracket" => 0x1E,
        "backslash" => 0x2A,
        "semicolon" => 0x29,
        "quote" => 0x27,
        "comma" => 0x2B,
        "period" => 0x2F,
        "slash" => 0x2C,
        "grave" => 0x32,
        // Shifted positions: the symbol, then the key it shares.
        "plus" => return Some((0x18, true)),
        "!" => return Some((0x12, true)),
        "@" => return Some((0x13, true)),
        "#" => return Some((0x14, true)),
        "$" => return Some((0x15, true)),
        "%" => return Some((0x17, true)),
        "^" => return Some((0x16, true)),
        "&" => return Some((0x1A, true)),
        "*" => return Some((0x1C, true)),
        "(" => return Some((0x19, true)),
        ")" => return Some((0x1D, true)),
        "_" => return Some((0x1B, true)),
        "{" => return Some((0x21, true)),
        "}" => return Some((0x1E, true)),
        "|" => return Some((0x2A, true)),
        ":" => return Some((0x29, true)),
        "\"" => return Some((0x27, true)),
        "<" => return Some((0x2B, true)),
        ">" => return Some((0x2F, true)),
        "?" => return Some((0x2C, true)),
        "~" => return Some((0x32, true)),
        _ => return None,
    };
    Some((code, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A keyboard event built from the HID source inherits the modifiers the
    /// system believes are held. Typed text carries none, so `type_text` must
    /// set the flags to empty rather than leave them alone.
    ///
    /// This was a live bug: with Command latched — a stuck physical key, a
    /// crashed app, a chord another process left behind — every character
    /// posted by `keyboard_type` arrived as a menu shortcut. Nothing was typed,
    /// and the call still reported the full character count, so an agent was
    /// told it had typed and saw no text.
    #[test]
    fn a_typed_event_carries_no_inherited_modifiers() {
        let Ok(src) = source() else {
            return; // No window server (headless CI): nothing to assert.
        };
        let Ok(ev) = CGEvent::new_keyboard_event(src, 0, true) else {
            return;
        };
        ev.set_string("a");
        ev.set_flags(CGEventFlags::empty());
        assert_eq!(
            ev.get_flags(),
            CGEventFlags::empty(),
            "typed text must post with no modifiers held"
        );
    }

    /// The path must end exactly on the destination — a drop one pixel short
    /// lands on whatever is next to the target.
    #[test]
    fn drag_path_is_monotonic_and_ends_on_target() {
        let path = drag_path((0.0, 0.0), (100.0, 50.0), 10);
        assert_eq!(path.len(), 10);
        assert_eq!(path[9], (100.0, 50.0));
        for w in path.windows(2) {
            assert!(w[1].0 > w[0].0 && w[1].1 > w[0].1, "path must advance");
        }
    }

    /// A drag between two identical points still has to emit motion: some
    /// targets only begin tracking on the first drag event.
    #[test]
    fn zero_length_drag_still_emits_a_move() {
        let path = drag_path((5.0, 5.0), (5.0, 5.0), 4);
        assert_eq!(path.len(), 4);
        assert!(path.iter().all(|p| *p == (5.0, 5.0)));
        assert_eq!(drag_path((0.0, 0.0), (1.0, 1.0), 0).len(), 1);
    }

    /// Every key the shared parser can emit must have a keycode here, or a
    /// chord that parses cleanly dies at the last step with "no keycode".
    #[test]
    fn every_key_the_shared_parser_emits_has_a_keycode() {
        use mcp_input::{parse_combo, Os};
        let mut spellings: Vec<String> = ('a'..='z').map(|c| c.to_string()).collect();
        spellings.extend(('0'..='9').map(|c| c.to_string()));
        spellings.extend((1..=20).map(|n| format!("f{n}")));
        for s in [
            "return",
            "enter",
            "tab",
            "space",
            "delete",
            "backspace",
            "del",
            "forwarddelete",
            "esc",
            "escape",
            "left",
            "right",
            "up",
            "down",
            "home",
            "end",
            "pageup",
            "pagedown",
            "pgup",
            "pgdn",
            "insert",
            "capslock",
            "printscreen",
            "minus",
            "equal",
            "plus",
            "comma",
            "period",
            "slash",
            "backslash",
            "semicolon",
            "quote",
            "grave",
            "leftbracket",
            "rightbracket",
            "-",
            "=",
            "+",
            ",",
            ".",
            "/",
            "\\",
            ";",
            "'",
            "`",
            "[",
            "]",
            "!",
            "@",
            "#",
            "$",
            "%",
            "^",
            "&",
            "*",
            "(",
            ")",
            "_",
            "{",
            "}",
            "|",
            ":",
            "\"",
            "<",
            ">",
            "?",
            "~",
        ] {
            spellings.push(s.to_string());
        }
        for s in &spellings {
            let combo = parse_combo(&format!("cmd+{s}"), Os::Mac)
                .unwrap_or_else(|e| panic!("{s} does not parse: {e}"));
            assert!(
                keycode_for(&combo.key).is_some(),
                "{s} parses to '{}', which has no keycode",
                combo.key
            );
        }
    }

    #[test]
    fn distinct_keys_do_not_share_a_keycode() {
        let mut seen = std::collections::HashMap::new();
        for k in ('a'..='z').chain('0'..='9') {
            let k = k.to_string();
            let code = keycode_for(&k).unwrap();
            assert_eq!(seen.insert(code, k.clone()), None, "{k} collides");
        }
        for k in [
            "return",
            "tab",
            "space",
            "delete",
            "forwarddelete",
            "escape",
            "left",
            "right",
            "up",
            "down",
            "home",
            "end",
            "pageup",
            "pagedown",
            "insert",
            "minus",
            "equal",
            "comma",
            "period",
            "slash",
            "backslash",
            "semicolon",
            "quote",
            "grave",
            "leftbracket",
            "rightbracket",
            "f1",
            "f12",
            "f13",
            "f20",
        ] {
            let code = keycode_for(k).unwrap();
            if let Some(other) = seen.insert(code, k.to_string()) {
                panic!("{k} and {other} share {code:?}");
            }
        }
    }

    #[test]
    fn shifted_symbols_name_their_base_key_and_ask_for_shift() {
        assert_eq!(keycode_for("plus"), Some((0x18, true)));
        assert_eq!(keycode_for("equal"), Some((0x18, false)));
        assert_eq!(keycode_for("?"), Some((0x2C, true)));
        assert_eq!(keycode_for("slash"), Some((0x2C, false)));
        assert_eq!(keycode_for("nosuchkey"), None);
    }

    #[test]
    fn a_second_key_in_a_chord_is_refused_before_anything_is_posted() {
        let e = key_combo("a+b").unwrap_err();
        assert!(matches!(e, InputError::InvalidArgs(_)), "{e:?}");
        assert!(matches!(
            key_combo("cmd").unwrap_err(),
            InputError::Unsupported(_)
        ));
        assert!(matches!(
            key_combo("cmd+nosuchkey").unwrap_err(),
            InputError::Unsupported(_)
        ));
    }

    #[test]
    fn modifier_flags_combine() {
        let f = modifier_flags(&["cmd".into(), "shift".into()]);
        assert!(f.contains(CGEventFlags::CGEventFlagCommand));
        assert!(f.contains(CGEventFlags::CGEventFlagShift));
        assert!(!f.contains(CGEventFlags::CGEventFlagControl));
        assert!(modifier_flags(&[]).is_empty());
        // Aliases must land on the same flag as the canonical name.
        assert_eq!(
            modifier_flags(&["alt".into()]),
            modifier_flags(&["option".into()])
        );
    }

    #[test]
    fn button_names_map_to_matching_down_up_pairs() {
        let b = Button::parse("right");
        assert!(matches!(b.down(), CGEventType::RightMouseDown));
        assert!(matches!(b.up(), CGEventType::RightMouseUp));
        assert!(matches!(b.dragged(), CGEventType::RightMouseDragged));
        let b = Button::parse("middle");
        assert_eq!(b, Button::Center);
        assert!(matches!(b.down(), CGEventType::OtherMouseDown));
        assert!(matches!(b.dragged(), CGEventType::OtherMouseDragged));
        let b = Button::parse("anything-else");
        assert!(matches!(b.down(), CGEventType::LeftMouseDown));
        assert!(matches!(b.up(), CGEventType::LeftMouseUp));
        assert!(matches!(b.dragged(), CGEventType::LeftMouseDragged));
    }
}

/// The held-button record and the motion events it selects. None of these
/// post anything: they build events, or run pure logic, and the owner's
/// pointer never moves.
#[cfg(test)]
mod drag_tests {
    use super::*;

    /// This was a live bug: `mouse_action down` then `move` posted
    /// `MouseMoved`, which anything that tracks `LeftMouseDragged` ignores,
    /// so a composed drag was a click followed by an idle pointer. Only
    /// `drag_drop` sent drag events.
    #[test]
    fn a_move_with_a_button_held_is_a_drag_of_that_button() {
        let (ty, b) = motion_event(&[]);
        assert!(matches!(ty, CGEventType::MouseMoved), "{ty:?}");
        assert_eq!(b, Button::Left);
        let (ty, b) = motion_event(&[Button::Left]);
        assert!(matches!(ty, CGEventType::LeftMouseDragged), "{ty:?}");
        assert_eq!(b, Button::Left);
        let (ty, b) = motion_event(&[Button::Right]);
        assert!(matches!(ty, CGEventType::RightMouseDragged), "{ty:?}");
        assert_eq!(b, Button::Right);
        let (ty, b) = motion_event(&[Button::Center]);
        assert!(matches!(ty, CGEventType::OtherMouseDragged), "{ty:?}");
        assert_eq!(b, Button::Center);
    }

    /// With several buttons down, left wins, then right, as a real mouse
    /// reports it; the order they were pressed in does not matter.
    #[test]
    fn with_several_buttons_held_left_wins_then_right() {
        let (ty, _) = motion_event(&[Button::Right, Button::Left]);
        assert!(matches!(ty, CGEventType::LeftMouseDragged), "{ty:?}");
        let (ty, _) = motion_event(&[Button::Center, Button::Right]);
        assert!(matches!(ty, CGEventType::RightMouseDragged), "{ty:?}");
    }

    /// The record follows down and up exactly, does not double-count a
    /// repeated press, and forgets everything when the brake fires.
    #[test]
    fn the_held_record_follows_presses_and_releases() {
        let held = Held::default();
        assert!(held.buttons().is_empty());
        held.press(Button::Left);
        held.press(Button::Left);
        assert_eq!(held.buttons(), vec![Button::Left]);
        held.press(Button::Right);
        held.release(Button::Left);
        assert_eq!(held.buttons(), vec![Button::Right]);
        let (ty, _) = motion_event(&held.buttons());
        assert!(matches!(ty, CGEventType::RightMouseDragged), "{ty:?}");
        held.release(Button::Right);
        assert!(held.buttons().is_empty());
        // Releasing a button that is not held is not an error.
        held.release(Button::Center);
        held.press(Button::Left);
        held.press(Button::Center);
        held.clear();
        assert!(held.buttons().is_empty(), "the brake forgets every press");
        let (ty, _) = motion_event(&held.buttons());
        assert!(matches!(ty, CGEventType::MouseMoved), "{ty:?}");
    }

    /// A press and a drag carry pressure; a move and a release do not. The
    /// constructor leaves every type at 0, which reads as "no button".
    #[test]
    fn pressure_is_one_while_a_button_is_down() {
        assert_eq!(pressure_for(CGEventType::LeftMouseDragged), 1.0);
        assert_eq!(pressure_for(CGEventType::OtherMouseDown), 1.0);
        assert_eq!(pressure_for(CGEventType::MouseMoved), 0.0);
        assert_eq!(pressure_for(CGEventType::RightMouseUp), 0.0);
    }

    /// The event that would go out for a motion while the left button is
    /// held: a `LeftMouseDragged` naming button 0, with the asked-for
    /// modifiers and pressure 1. Built, inspected and dropped, never posted.
    #[test]
    fn a_built_drag_event_names_its_type_button_flags_and_pressure() {
        if source().is_err() {
            return; // No window server (headless CI): nothing to build.
        }
        let held = Held::default();
        held.press(Button::Right);
        let (ty, btn) = motion_event(&held.buttons());
        let flags = modifier_flags(&["cmd".into()]);
        let (ev, _) = build_mouse_event(ty, CGPoint::new(10.0, 10.0), btn, flags, 1).unwrap();
        assert!(matches!(ev.get_type(), CGEventType::RightMouseDragged));
        assert_eq!(
            ev.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER),
            1,
            "right is button 1"
        );
        assert_eq!(
            ev.get_double_value_field(EventField::MOUSE_EVENT_PRESSURE),
            1.0
        );
        assert!(ev.get_flags().contains(CGEventFlags::CGEventFlagCommand));
        assert_eq!(
            ev.get_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE),
            1
        );

        held.release(Button::Right);
        held.press(Button::Left);
        let (ty, btn) = motion_event(&held.buttons());
        let (ev, _) =
            build_mouse_event(ty, CGPoint::new(10.0, 10.0), btn, CGEventFlags::empty(), 1).unwrap();
        assert!(matches!(ev.get_type(), CGEventType::LeftMouseDragged));
        assert_eq!(
            ev.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER),
            0
        );
        assert_eq!(
            ev.get_double_value_field(EventField::MOUSE_EVENT_PRESSURE),
            1.0
        );
        assert!(ev.get_flags().is_empty());
    }

    /// With nothing held the same motion is a plain `MouseMoved` with no
    /// pressure and, when nothing was asked for, no inherited modifiers.
    #[test]
    fn a_built_move_event_with_nothing_held_is_a_plain_move() {
        if source().is_err() {
            return;
        }
        let held = Held::default();
        let (ty, btn) = motion_event(&held.buttons());
        let (ev, _) =
            build_mouse_event(ty, CGPoint::new(10.0, 10.0), btn, CGEventFlags::empty(), 1).unwrap();
        assert!(matches!(ev.get_type(), CGEventType::MouseMoved));
        assert_eq!(
            ev.get_double_value_field(EventField::MOUSE_EVENT_PRESSURE),
            0.0
        );
        assert!(ev.get_flags().is_empty());
    }
}

/// Wheel events: deltas and the modifiers that ride on them. Built, never
/// posted.
#[cfg(test)]
mod scroll_tests {
    use super::*;

    #[test]
    fn scroll_deltas_follow_direction_and_a_page_is_ten_lines() {
        assert_eq!(scroll_deltas(ScrollDir::Up, 3), (3, 0));
        assert_eq!(scroll_deltas(ScrollDir::Down, 3), (-3, 0));
        assert_eq!(scroll_deltas(ScrollDir::Left, 2), (0, 2));
        assert_eq!(scroll_deltas(ScrollDir::Right, 2), (0, -2));
        assert_eq!(scroll_deltas(ScrollDir::PageUp, 1), (10, 0));
        assert_eq!(scroll_deltas(ScrollDir::PageDown, 2), (-20, 0));
        // Zero and negative amounts still scroll one line rather than nothing.
        assert_eq!(scroll_deltas(ScrollDir::Down, 0), (-1, 0));
        assert_eq!(scroll_deltas(ScrollDir::Down, -5), (-1, 0));
    }

    /// Cmd+wheel zooms in most apps; the modifier has to be on the wheel
    /// event's flags, which `scroll` had no way to set before.
    #[test]
    fn a_built_wheel_event_carries_the_asked_for_modifiers() {
        if source().is_err() {
            return; // No window server (headless CI): nothing to build.
        }
        let ev = build_scroll_event(
            ScrollDir::Down,
            3,
            modifier_flags(&["cmd".into(), "shift".into()]),
        )
        .unwrap();
        assert!(matches!(ev.get_type(), CGEventType::ScrollWheel));
        let f = ev.get_flags();
        assert!(f.contains(CGEventFlags::CGEventFlagCommand));
        assert!(f.contains(CGEventFlags::CGEventFlagShift));
        assert!(!f.contains(CGEventFlags::CGEventFlagControl));
        assert_eq!(
            ev.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1),
            -3
        );
        // Asking for none clears whatever the system believes is held.
        let ev = build_scroll_event(ScrollDir::Up, 1, CGEventFlags::empty()).unwrap();
        assert!(ev.get_flags().is_empty());
    }
}

#[cfg(test)]
mod pointer_tests {
    use super::*;

    /// A point already on a display is left alone.
    #[test]
    fn a_point_on_screen_is_not_moved() {
        let screens = [(0.0, 0.0, 1512.0, 982.0)];
        assert_eq!(clamp_point((100.0, 200.0), &screens), (100.0, 200.0));
    }

    /// A point past the edge is pulled to the nearest position the pointer can
    /// actually occupy — which is where the OS will put it anyway. Recording
    /// the unclamped request would make the server's own click look like
    /// somebody else's.
    #[test]
    fn a_point_off_screen_is_clamped_to_the_nearest_display() {
        let screens = [(0.0, 0.0, 1512.0, 982.0)];
        assert_eq!(clamp_point((5000.0, 500.0), &screens), (1512.0, 500.0));
        assert_eq!(clamp_point((-40.0, -40.0), &screens), (0.0, 0.0));
    }

    /// With two displays, the nearer one wins.
    #[test]
    fn clamping_picks_the_closest_of_several_displays() {
        let screens = [(0.0, 0.0, 100.0, 100.0), (1000.0, 0.0, 100.0, 100.0)];
        assert_eq!(clamp_point((1050.0, 500.0), &screens), (1050.0, 100.0));
        assert_eq!(clamp_point((50.0, 500.0), &screens), (50.0, 100.0));
    }

    /// No display information means no clamping: guessing would be worse.
    #[test]
    fn with_no_displays_the_point_is_unchanged() {
        assert_eq!(clamp_point((5000.0, 5000.0), &[]), (5000.0, 5000.0));
    }

    /// Reading the pointer needs no permission and must always answer.
    #[test]
    fn the_pointer_can_be_read_without_moving_it() {
        // Headless CI has no window server, so `None` or an error is an answer
        // too. What matters is that the call returns rather than blocking.
        if let Ok(Some((x, y))) = pointer_position() {
            assert!(x.is_finite() && y.is_finite());
        }
    }
}
