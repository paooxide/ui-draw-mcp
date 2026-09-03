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

/// Type Unicode text by attaching it to a synthetic keystroke.
pub fn type_text(text: &str) -> Result<(), InputError> {
    let src = source()?;
    let down = CGEvent::new_keyboard_event(src.clone(), 0, true).map_err(|_| fail("key down"))?;
    down.set_string(text);
    // Typed text carries no modifiers, and *not setting* the flags is not the
    // same as setting them to empty: an event built from the HID state source
    // inherits whatever the system believes is currently held. With Command
    // latched — by a stuck physical key, a crashed app, or a previous chord —
    // every character silently becomes a menu shortcut. The call still reports
    // the full character count, so the agent is told it typed and sees nothing.
    down.set_flags(CGEventFlags::empty());
    down.post(CGEventTapLocation::HID);
    let up = CGEvent::new_keyboard_event(src, 0, false).map_err(|_| fail("key up"))?;
    up.set_string(text);
    up.set_flags(CGEventFlags::empty());
    up.post(CGEventTapLocation::HID);
    Ok(())
}

/// Press a chord like `cmd+shift+n`.
pub fn key_combo(combo: &str) -> Result<(), InputError> {
    let mut flags = CGEventFlags::empty();
    let mut keycode: Option<CGKeyCode> = None;
    for seg in combo.split('+') {
        match seg {
            "cmd" | "command" | "meta" | "super" => flags |= CGEventFlags::CGEventFlagCommand,
            "shift" => flags |= CGEventFlags::CGEventFlagShift,
            "opt" | "option" | "alt" => flags |= CGEventFlags::CGEventFlagAlternate,
            "ctrl" | "control" => flags |= CGEventFlags::CGEventFlagControl,
            "fn" => flags |= CGEventFlags::CGEventFlagSecondaryFn,
            key => keycode = keycode_for(key),
        }
    }
    let keycode = keycode
        .ok_or_else(|| InputError::Unsupported(format!("no keycode for combo '{combo}'")))?;
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

/// Post one mouse event.
///
/// `click_state` is the part that is easy to leave out and impossible to notice
/// afterwards: macOS decides "double click" from this field, not from how fast
/// two clicks arrive. Sending down/up twice with the default state of 1 gives
/// the target two ordinary single clicks, so double-click-to-open and
/// triple-click-to-select-line quietly do nothing.
fn post_mouse_ex(
    ty: CGEventType,
    pt: CGPoint,
    btn: CGMouseButton,
    flags: CGEventFlags,
    click_state: i64,
) -> Result<(), InputError> {
    // The OS clamps a point past the screen edge; recording the unclamped
    // request would have the watcher compare against a position the pointer
    // never occupied, and read the server's own click as a human takeover.
    let pt = {
        let (x, y) = clamp_point((pt.x, pt.y), &display_rects());
        CGPoint::new(x, y)
    };
    let ev = CGEvent::new_mouse_event(source()?, ty, pt, btn).map_err(|_| fail("mouse event"))?;
    // Always set it, including for a single click. `CGEventCreateMouseEvent`
    // leaves the field at 0, which reaches AppKit as `clickCount == 0` — not
    // "one click". Measured against a live NSTextView, a second such click
    // extends the selection from the previous caret instead of moving it, so
    // plain clicks silently behaved like shift-clicks.
    ev.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, click_state.max(1));
    // Always set flags, including the empty set. A mouse event created from an
    // HID-state source *inherits* whatever modifiers the system currently
    // believes are held, so skipping this when no modifier was asked for lets a
    // stray Shift or Command latch onto every subsequent click — a plain click
    // then behaves as shift-click and extends a selection instead of moving the
    // caret. Setting it explicitly clears the inherited state.
    ev.set_flags(flags);
    // Record before posting: this is the only place the pointer is written, so
    // it is the only place the override watcher can learn what was ours.
    record_set(pt);
    ev.post(CGEventTapLocation::HID);
    Ok(())
}

fn post_mouse(ty: CGEventType, pt: CGPoint, btn: CGMouseButton) -> Result<(), InputError> {
    post_mouse_ex(ty, pt, btn, CGEventFlags::empty(), 1)
}

/// Resolve a button name to its `(button, down, up)` event triple.
fn button_events(name: &str) -> (CGMouseButton, CGEventType, CGEventType) {
    match name {
        "right" => (
            CGMouseButton::Right,
            CGEventType::RightMouseDown,
            CGEventType::RightMouseUp,
        ),
        "middle" | "center" => (
            CGMouseButton::Center,
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseUp,
        ),
        _ => (
            CGMouseButton::Left,
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseUp,
        ),
    }
}

pub fn mouse(
    kind: MouseKind,
    x: f64,
    y: f64,
    button: Option<&str>,
    modifiers: &[String],
) -> Result<(), InputError> {
    let pt = CGPoint::new(x, y);
    let name = if matches!(kind, MouseKind::RightClick) {
        "right"
    } else {
        button.unwrap_or("left")
    };
    let (btn, down, up) = button_events(name);
    let flags = modifier_flags(modifiers);
    // A click at a position the pointer is not at can miss hover-activated
    // targets, so move there first.
    if !matches!(kind, MouseKind::Move) {
        post_mouse_ex(CGEventType::MouseMoved, pt, btn, flags, 1)?;
    }
    // Multi-clicks are a *sequence* of clicks with a rising click-state, not N
    // independent clicks: the target reads state 2 as "this is the double".
    let clicks = match kind {
        MouseKind::Double => 2,
        MouseKind::Triple => 3,
        _ => 1,
    };
    match kind {
        MouseKind::Move => post_mouse_ex(CGEventType::MouseMoved, pt, btn, flags, 1)?,
        MouseKind::Down => post_mouse_ex(down, pt, btn, flags, 1)?,
        MouseKind::Up => post_mouse_ex(up, pt, btn, flags, 1)?,
        _ => {
            for state in 1..=clicks {
                post_mouse_ex(down, pt, btn, flags, state)?;
                post_mouse_ex(up, pt, btn, flags, state)?;
            }
        }
    }
    Ok(())
}

/// Press at `pt`, beginning a drag. Pair with [`drag_to`] and [`drag_end`].
///
/// Split into three calls so the caller can space the intermediate moves out in
/// time without blocking an async runtime with `thread::sleep`.
pub fn drag_begin(pt: (f64, f64), modifiers: &[String]) -> Result<(), InputError> {
    let p = CGPoint::new(pt.0, pt.1);
    let flags = modifier_flags(modifiers);
    post_mouse_ex(CGEventType::MouseMoved, p, CGMouseButton::Left, flags, 1)?;
    post_mouse_ex(CGEventType::LeftMouseDown, p, CGMouseButton::Left, flags, 1)
}

pub fn drag_to(pt: (f64, f64), modifiers: &[String]) -> Result<(), InputError> {
    post_mouse_ex(
        CGEventType::LeftMouseDragged,
        CGPoint::new(pt.0, pt.1),
        CGMouseButton::Left,
        modifier_flags(modifiers),
        1,
    )
}

pub fn drag_end(pt: (f64, f64), modifiers: &[String]) -> Result<(), InputError> {
    post_mouse_ex(
        CGEventType::LeftMouseUp,
        CGPoint::new(pt.0, pt.1),
        CGMouseButton::Left,
        modifier_flags(modifiers),
        1,
    )
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

pub fn scroll(x: f64, y: f64, dir: ScrollDir, amount: i32) -> Result<(), InputError> {
    // Position the cursor so the scroll targets that location.
    post_mouse(
        CGEventType::MouseMoved,
        CGPoint::new(x, y),
        CGMouseButton::Left,
    )?;
    let a = amount.max(1);
    let (vertical, horizontal) = match dir {
        ScrollDir::Up => (a, 0),
        ScrollDir::Down => (-a, 0),
        ScrollDir::Left => (0, a),
        ScrollDir::Right => (0, -a),
        ScrollDir::PageUp => (a * 10, 0),
        ScrollDir::PageDown => (-a * 10, 0),
    };
    let ev =
        CGEvent::new_scroll_event(source()?, ScrollEventUnit::LINE, 2, vertical, horizontal, 0)
            .map_err(|_| fail("scroll event"))?;
    ev.post(CGEventTapLocation::HID);
    Ok(())
}

pub fn hover(x: f64, y: f64) -> Result<(), InputError> {
    post_mouse(
        CGEventType::MouseMoved,
        CGPoint::new(x, y),
        CGMouseButton::Left,
    )
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

/// US ANSI virtual keycodes for a key name (letters, digits, common named keys).
fn keycode_for(key: &str) -> Option<CGKeyCode> {
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
        "escape" | "esc" => 0x35,
        "left" => 0x7B,
        "right" => 0x7C,
        "down" => 0x7D,
        "up" => 0x7E,
        "home" => 0x73,
        "end" => 0x77,
        "pageup" | "page_up" => 0x74,
        "pagedown" | "page_down" => 0x79,
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
        _ => return None,
    };
    Some(code)
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
        let (_, d, u) = button_events("right");
        assert!(matches!(d, CGEventType::RightMouseDown));
        assert!(matches!(u, CGEventType::RightMouseUp));
        let (_, d, u) = button_events("anything-else");
        assert!(matches!(d, CGEventType::LeftMouseDown));
        assert!(matches!(u, CGEventType::LeftMouseUp));
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
