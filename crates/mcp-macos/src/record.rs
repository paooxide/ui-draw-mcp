//! Listening to a person: a listen-only event tap, and the accessibility facts
//! about whatever they touched.
//!
//! The tap is `kCGEventTapOptionListenOnly`: it sees mouse-down and key-down
//! events and can neither change nor swallow them. It needs the Input
//! Monitoring permission (without which the system returns no tap, and the
//! preflight here says so before asking for anything) and Accessibility for the
//! element reads. The callback does as little as possible, copying the event
//! into a channel; a second thread asks accessibility what was under the
//! pointer or focused, because a slow answer in the callback would delay
//! events for the whole session and the system disables a tap that does.
//!
//! One session owns both threads and stops them when it is dropped.

use std::os::raw::{c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use core_foundation::base::{CFType, CFTypeRef, TCFType};
use core_foundation::runloop::{kCFRunLoopCommonModes, kCFRunLoopDefaultMode, CFRunLoop};
use core_graphics::event::{
    CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType, CallbackResult, EventField,
};
use foreign_types::ForeignType;
use mcp_input::record::{
    scrub, trim_ancestry, Captured, Crumb, Facts, Input, RecordBackend, RecordError, RecordSession,
};

use crate::imp::{
    app_element, copy_attr, copy_attr_string, window_owners, AXUIElementCreateSystemWide,
    AXUIElementGetPid, AXUIElementSetMessagingTimeout, MacosBackend,
};

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    /// Preflight, never prompt: whether Input Monitoring is granted.
    fn CGPreflightListenEventAccess() -> bool;
    fn CGEventKeyboardGetUnicodeString(
        event: *mut c_void,
        max_len: usize,
        actual_len: *mut usize,
        buf: *mut u16,
    );
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> u8;
    fn AXUIElementCopyElementAtPosition(
        application: CFTypeRef,
        x: f32,
        y: f32,
        element: *mut CFTypeRef,
    ) -> i32;
}

/// How long one accessibility read may take before the event is kept without
/// its element.
const READ_TIMEOUT_S: f32 = 0.3;
/// Parents read above an element; [`trim_ancestry`] cuts it shorter.
const PARENTS_READ: usize = 12;
/// How often the threads look for the stop flag.
const SLICE: Duration = Duration::from_millis(100);

/// What the tap callback copies out of an event.
enum Raw {
    Click {
        button: u8,
        clicks: u32,
        x: f64,
        y: f64,
        at_ms: u64,
    },
    Key {
        code: u16,
        text: String,
        cmd: bool,
        ctrl: bool,
        alt: bool,
        shift: bool,
        at_ms: u64,
    },
}

/// A running tap and the thread that reads the elements.
pub struct TapSession {
    out: mpsc::Receiver<Captured>,
    lost: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl RecordSession for TapSession {
    fn drain(&mut self) -> Vec<Captured> {
        self.out.try_iter().collect()
    }

    fn lost(&self) -> usize {
        self.lost.load(Ordering::SeqCst)
    }
}

impl Drop for TapSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl RecordBackend for MacosBackend {
    fn start_recording(&self) -> Result<Box<dyn RecordSession>, RecordError> {
        // SAFETY: plain preflight calls with no arguments.
        unsafe {
            if !CGPreflightListenEventAccess() {
                return Err(RecordError::PermissionDenied(
                    "Input Monitoring is not granted: add the app that runs agentctl (your terminal \
                     or editor) under System Settings > Privacy & Security > Input Monitoring"
                        .into(),
                ));
            }
            if AXIsProcessTrusted() == 0 {
                return Err(RecordError::PermissionDenied(
                    "Accessibility is not granted: recording needs it to tell what was clicked; add the \
                     app that runs agentctl under System Settings > Privacy & Security > Accessibility"
                        .into(),
                ));
            }
        }
        let started = Instant::now();
        let stop = Arc::new(AtomicBool::new(false));
        let lost = Arc::new(AtomicUsize::new(0));
        let (raw_tx, raw_rx) = mpsc::channel::<Raw>();
        let (out_tx, out_rx) = mpsc::channel::<Captured>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), RecordError>>();

        // The tap and its run loop live on one thread, which owns them until
        // the stop flag is raised.
        let tap_thread = {
            let (stop, lost) = (stop.clone(), lost.clone());
            std::thread::Builder::new()
                .name("agentctl-record-tap".into())
                .spawn(move || run_tap(started, raw_tx, stop, lost, ready_tx))
                .map_err(|e| RecordError::Failed(format!("could not start the listener: {e}")))?
        };
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = tap_thread.join();
                return Err(e);
            }
            Err(_) => {
                stop.store(true, Ordering::SeqCst);
                let _ = tap_thread.join();
                return Err(RecordError::Failed(
                    "the event tap did not start within 5 s".into(),
                ));
            }
        }
        let resolver = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("agentctl-record-resolve".into())
                .spawn(move || resolve_loop(raw_rx, out_tx, stop))
                .map_err(|e| RecordError::Failed(format!("could not start the reader: {e}")))?
        };
        Ok(Box::new(TapSession {
            out: out_rx,
            lost,
            stop,
            threads: vec![tap_thread, resolver],
        }))
    }
}

/// The tap thread: create the tap, run its loop, and take it down.
fn run_tap(
    started: Instant,
    tx: mpsc::Sender<Raw>,
    stop: Arc<AtomicBool>,
    lost: Arc<AtomicUsize>,
    ready: mpsc::Sender<Result<(), RecordError>>,
) {
    let lost_in_cb = lost.clone();
    let tap = CGEventTap::new(
        CGEventTapLocation::Session,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::ListenOnly,
        vec![
            CGEventType::LeftMouseDown,
            CGEventType::RightMouseDown,
            CGEventType::OtherMouseDown,
            CGEventType::KeyDown,
        ],
        move |_proxy, kind, ev| {
            let at_ms = started.elapsed().as_millis() as u64;
            let raw = match kind {
                CGEventType::LeftMouseDown
                | CGEventType::RightMouseDown
                | CGEventType::OtherMouseDown => {
                    let p = ev.location();
                    Raw::Click {
                        button: match kind {
                            CGEventType::LeftMouseDown => 0,
                            CGEventType::RightMouseDown => 1,
                            _ => 2,
                        },
                        clicks: ev
                            .get_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE)
                            .max(1) as u32,
                        x: p.x,
                        y: p.y,
                        at_ms,
                    }
                }
                CGEventType::KeyDown => {
                    let flags = ev.get_flags();
                    let mut buf = [0u16; 64];
                    let mut n = 0usize;
                    // SAFETY: the event is valid for the call, and the buffer
                    // holds the 64 units the call is told it may write.
                    unsafe {
                        CGEventKeyboardGetUnicodeString(
                            ev.as_ptr() as *mut c_void,
                            buf.len(),
                            &mut n,
                            buf.as_mut_ptr(),
                        );
                    }
                    Raw::Key {
                        code: ev.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16,
                        text: String::from_utf16_lossy(&buf[..n.min(buf.len())]),
                        cmd: flags.contains(CGEventFlags::CGEventFlagCommand),
                        ctrl: flags.contains(CGEventFlags::CGEventFlagControl),
                        alt: flags.contains(CGEventFlags::CGEventFlagAlternate),
                        shift: flags.contains(CGEventFlags::CGEventFlagShift),
                        at_ms,
                    }
                }
                // The system turns a tap off when it is too slow; the events
                // after that are lost, and the count says so.
                _ => {
                    lost_in_cb.fetch_add(1, Ordering::SeqCst);
                    return CallbackResult::Keep;
                }
            };
            if tx.send(raw).is_err() {
                lost_in_cb.fetch_add(1, Ordering::SeqCst);
            }
            CallbackResult::Keep
        },
    );
    let tap = match tap {
        Ok(t) => t,
        Err(()) => {
            let _ = ready.send(Err(RecordError::Failed(
                "the system would not create an event tap: Input Monitoring may have been revoked, or \
                 another process holds secure input"
                    .into(),
            )));
            return;
        }
    };
    let Ok(source) = tap.mach_port().create_runloop_source(0) else {
        let _ = ready.send(Err(RecordError::Failed(
            "the event tap has no run-loop source".into(),
        )));
        return;
    };
    let run_loop = CFRunLoop::get_current();
    // SAFETY: the constant is a valid CFString for the process's lifetime.
    unsafe { run_loop.add_source(&source, kCFRunLoopCommonModes) };
    tap.enable();
    let _ = ready.send(Ok(()));
    while !stop.load(Ordering::SeqCst) {
        // SAFETY: as above.
        unsafe { CFRunLoop::run_in_mode(kCFRunLoopDefaultMode, SLICE, false) };
    }
    // The tap goes with this scope; the source comes off the loop first.
    // SAFETY: as above.
    unsafe { run_loop.remove_source(&source, kCFRunLoopCommonModes) };
    drop(tap);
}

/// The reader thread: ask accessibility about each event and pass it on.
fn resolve_loop(rx: mpsc::Receiver<Raw>, tx: mpsc::Sender<Captured>, stop: Arc<AtomicBool>) {
    loop {
        match rx.recv_timeout(SLICE) {
            Ok(raw) => {
                // SAFETY: synchronous AX reads; nothing is held across a call.
                let mut c = unsafe { capture(raw) };
                // Gone before it leaves this thread if it was a secret.
                scrub(&mut c);
                if tx.send(c).is_err() {
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

unsafe fn capture(raw: Raw) -> Captured {
    match raw {
        Raw::Click {
            button,
            clicks,
            x,
            y,
            at_ms,
        } => Captured {
            at_ms,
            facts: element_at(x, y).and_then(|e| facts_of(&e)),
            input: Input::Click {
                button,
                clicks,
                x,
                y,
            },
        },
        Raw::Key {
            code,
            text,
            cmd,
            ctrl,
            alt,
            shift,
            at_ms,
        } => Captured {
            at_ms,
            facts: focused_element().and_then(|e| facts_of(&e)),
            input: Input::Key {
                name: crate::event::key_name_for(code).map(str::to_string),
                text,
                cmd,
                ctrl,
                alt,
                shift,
            },
        },
    }
}

/// The accessibility element under a screen point.
unsafe fn element_at(x: f64, y: f64) -> Option<CFType> {
    let sys = CFType::wrap_under_create_rule(AXUIElementCreateSystemWide());
    let mut out: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyElementAtPosition(sys.as_CFTypeRef(), x as f32, y as f32, &mut out);
    (err == 0 && !out.is_null()).then(|| CFType::wrap_under_create_rule(out))
}

/// The element with the keyboard focus, in the application that has it.
unsafe fn focused_element() -> Option<CFType> {
    let sys = CFType::wrap_under_create_rule(AXUIElementCreateSystemWide());
    AXUIElementSetMessagingTimeout(sys.as_CFTypeRef(), READ_TIMEOUT_S);
    copy_attr(sys.as_CFTypeRef(), "AXFocusedUIElement")
}

/// An element's name as the tree shows it: its title, else its description.
unsafe fn title_of(el: CFTypeRef) -> Option<String> {
    copy_attr_string(el, "AXTitle")
        .filter(|s| !s.is_empty())
        .or_else(|| copy_attr_string(el, "AXDescription").filter(|s| !s.is_empty()))
}

/// What an element is, where it sits, and whose it is.
unsafe fn facts_of(el: &CFType) -> Option<Facts> {
    let e = el.as_CFTypeRef();
    AXUIElementSetMessagingTimeout(e, READ_TIMEOUT_S);
    let mut pid: c_int = 0;
    AXUIElementGetPid(e, &mut pid);
    if pid <= 0 {
        return None;
    }
    // The application is named as the tree names it, so a locator finds it.
    let app = app_element(pid)
        .and_then(|a| copy_attr_string(a.as_CFTypeRef(), "AXTitle"))
        .filter(|s| !s.is_empty())
        .or_else(|| {
            window_owners()
                .into_iter()
                .find(|(p, _)| *p == pid)
                .map(|(_, n)| n)
        })?;
    let subrole = copy_attr_string(e, "AXSubrole");
    let mut chain: Vec<Crumb> = Vec::new();
    let mut cur = CFType::wrap_under_get_rule(e);
    for _ in 0..PARENTS_READ {
        let Some(parent) = copy_attr(cur.as_CFTypeRef(), "AXParent") else {
            break;
        };
        AXUIElementSetMessagingTimeout(parent.as_CFTypeRef(), READ_TIMEOUT_S);
        chain.push(Crumb {
            role: copy_attr_string(parent.as_CFTypeRef(), "AXRole").unwrap_or_default(),
            title: title_of(parent.as_CFTypeRef()),
        });
        cur = parent;
    }
    let ancestry = trim_ancestry(chain);
    let window = ancestry
        .iter()
        .find(|c| mcp_a11y::normalize_role(&c.role) == "window")
        .and_then(|c| c.title.clone());
    Some(Facts {
        app,
        window,
        role: copy_attr_string(e, "AXRole").unwrap_or_default(),
        title: title_of(e),
        ancestry,
        secure: subrole.as_deref() == Some("AXSecureTextField"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The permission answer is a real one: either a tap starts and stops
    /// cleanly, or the call says which permission is missing. Nothing is
    /// recorded or kept.
    #[test]
    fn starting_a_tap_either_works_or_names_the_missing_permission() {
        let b = MacosBackend::new();
        match b.start_recording() {
            Ok(mut s) => {
                assert!(s.drain().len() < 1000);
                drop(s);
            }
            Err(RecordError::PermissionDenied(m)) => {
                assert!(
                    m.contains("Input Monitoring") || m.contains("Accessibility"),
                    "{m}"
                );
            }
            Err(e) => panic!("unexpected: {e:?}"),
        }
    }
}
