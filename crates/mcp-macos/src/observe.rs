//! Hearing about UI changes from the app itself, instead of asking on a timer.
//!
//! An `AXObserver` delivers accessibility notifications (value changed, window
//! created, focus moved...) through a run-loop source. [`watch_pid`] puts one
//! on a thread of its own, whose only job is to run that loop and wake a
//! `Notify` when anything arrives. The waiting code then re-checks its
//! condition on the wake instead of every 150 ms.
//!
//! Everything started here has exactly one owner, [`ObserverGuard`], and one
//! way out: dropping it. The thread does its own teardown (the notifications
//! removed, the source taken off the loop, the observer released, the callback
//! context freed) because those must happen on the thread that owns the loop,
//! and in that order, while nothing else can be running the callback.

use std::os::raw::{c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use core_foundation::base::{CFType, CFTypeRef, TCFType};
use core_foundation::runloop::{kCFRunLoopDefaultMode, CFRunLoop, CFRunLoopRunResult};
use core_foundation::string::{CFString, CFStringRef};
use core_foundation_sys::runloop::CFRunLoopSourceRef;
use mcp_window::ChangeWatch;
use tokio::sync::{oneshot, Notify};

use crate::ax_actions::registration_outcome;
use crate::imp::{
    AXError, AXUIElementCreateApplication, AXUIElementSetMessagingTimeout, KAX_SUCCESS,
};

type AXObserverRef = CFTypeRef;
type AXObserverCallback = unsafe extern "C" fn(
    observer: AXObserverRef,
    element: CFTypeRef,
    notification: CFStringRef,
    refcon: *mut c_void,
);

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    /// Create rule: the observer is owned by the caller.
    fn AXObserverCreate(
        application: c_int,
        callback: AXObserverCallback,
        observer: *mut AXObserverRef,
    ) -> AXError;
    fn AXObserverAddNotification(
        observer: AXObserverRef,
        element: CFTypeRef,
        notification: CFStringRef,
        refcon: *mut c_void,
    ) -> AXError;
    fn AXObserverRemoveNotification(
        observer: AXObserverRef,
        element: CFTypeRef,
        notification: CFStringRef,
    ) -> AXError;
    /// Get rule: the source belongs to the observer and lives as long as it.
    fn AXObserverGetRunLoopSource(observer: AXObserverRef) -> CFRunLoopSourceRef;
}

/// What to listen for. Registered on the application element, which receives
/// these from its windows and their contents. Each is a cheap hint: the wait
/// re-reads the tree and judges the condition itself, so a notification that
/// turns out to mean nothing costs one look.
const NOTIFICATIONS: &[&str] = &[
    "AXValueChanged",
    "AXFocusedUIElementChanged",
    "AXFocusedWindowChanged",
    "AXMainWindowChanged",
    "AXWindowCreated",
    "AXUIElementDestroyed",
    "AXTitleChanged",
    "AXSelectedTextChanged",
    "AXLayoutChanged",
    "AXMenuOpened",
    "AXMenuClosed",
];

/// How long the loop runs before it looks at the stop flag on its own. A
/// stop request also interrupts the loop directly; this bounds the rare case
/// where that arrived just before the loop began.
const LOOP_SLICE: Duration = Duration::from_millis(100);
/// How long to wait for the thread to say whether the app can be observed.
const SETUP_TIMEOUT: Duration = Duration::from_secs(3);
/// Bound on each request the thread makes of the app while registering.
const APP_REQUEST_TIMEOUT_S: f32 = 2.0;

/// Observer threads currently alive (from before the first AX call to after the
/// last release). Tests read it to prove that teardown is complete; it costs
/// two atomic operations per watch.
static LIVE_THREADS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Counts one thread in [`LIVE_THREADS`] for as long as it is held.
struct Alive;

impl Alive {
    fn new() -> Alive {
        LIVE_THREADS.fetch_add(1, Ordering::SeqCst);
        Alive
    }
}

impl Drop for Alive {
    fn drop(&mut self) {
        LIVE_THREADS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The observer callback. `refcon` is the `Arc<Notify>` pointer the thread
/// leaked on purpose for the observer's life; it is borrowed here and freed
/// by the thread only after the observer is gone, and callbacks run only on
/// that thread, so it is always valid when this runs.
unsafe extern "C" fn on_notification(
    _observer: AXObserverRef,
    _element: CFTypeRef,
    _notification: CFStringRef,
    refcon: *mut c_void,
) {
    if let Some(notify) = (refcon as *const Notify).as_ref() {
        // A permit is stored if nobody is waiting yet, so a notification
        // between two looks is never lost, and a burst is one wake-up.
        notify.notify_one();
    }
}

/// Owns an observer thread. Dropping it stops the loop, waits for the thread
/// to finish tearing down, and leaves nothing running.
pub(crate) struct ObserverGuard {
    stop: Arc<AtomicBool>,
    run_loop: CFRunLoop,
    thread: Option<JoinHandle<()>>,
}

impl Drop for ObserverGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Thread-safe by documentation, and a no-op if the loop is between
        // slices, which the flag above then catches.
        self.run_loop.stop();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Observe `pid`'s UI. `None` if the app refuses (no accessibility permission,
/// an app that accepts no notifications, one that does not answer in time), in
/// which case nothing is left running and the caller polls.
pub(crate) async fn watch_pid(pid: i32) -> Option<ChangeWatch> {
    let notify = Arc::new(Notify::new());
    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = oneshot::channel();
    let thread = {
        let (notify, stop) = (notify.clone(), stop.clone());
        // Counted from here rather than inside the thread, so a thread that has
        // been spawned and not yet scheduled is already accounted for.
        let alive = Alive::new();
        std::thread::Builder::new()
            .name("agentctl-ax-observer".into())
            .spawn(move || {
                let _alive = alive;
                observe(pid, notify, stop, ready_tx)
            })
            .ok()?
    };
    match tokio::time::timeout(SETUP_TIMEOUT, ready_rx).await {
        Ok(Ok(Ok(run_loop))) => Some(ChangeWatch::new(
            notify,
            ObserverGuard {
                stop,
                run_loop,
                thread: Some(thread),
            },
        )),
        Ok(Ok(Err(why))) => {
            tracing::debug!(pid, %why, "app cannot be observed; waits will poll");
            // The thread has sent its answer and is returning.
            let _ = thread.join();
            None
        }
        _ => {
            // Still inside a request to a slow app. Tell it to leave as soon
            // as it is out; it cleans up after itself and is not waited for.
            stop.store(true, Ordering::SeqCst);
            tracing::debug!(
                pid,
                "app did not answer observation setup in time; waits will poll"
            );
            None
        }
    }
}

/// The observer thread: set up, run the loop until told to stop, tear down.
fn observe(
    pid: i32,
    notify: Arc<Notify>,
    stop: Arc<AtomicBool>,
    ready: oneshot::Sender<Result<CFRunLoop, String>>,
) {
    // One strong count lent to the C side for as long as the observer exists.
    let refcon = Arc::into_raw(notify) as *mut c_void;
    // SAFETY: every pointer below comes from the call that made it, is used
    // only on this thread, and is released exactly once, in the order at the
    // bottom. `refcon` is reclaimed last, after the observer (and so any
    // callback) is gone.
    unsafe {
        let mut raw: AXObserverRef = ptr::null();
        let err = AXObserverCreate(pid, on_notification, &mut raw);
        if err != KAX_SUCCESS || raw.is_null() {
            drop(Arc::from_raw(refcon as *const Notify));
            let _ = ready.send(Err(format!("AXObserverCreate returned {err}")));
            return;
        }
        let observer = CFType::wrap_under_create_rule(raw);

        let app_raw = AXUIElementCreateApplication(pid);
        if app_raw.is_null() {
            drop(observer);
            drop(Arc::from_raw(refcon as *const Notify));
            let _ = ready.send(Err("no accessibility element for the app".into()));
            return;
        }
        let app = CFType::wrap_under_create_rule(app_raw);
        AXUIElementSetMessagingTimeout(app.as_CFTypeRef(), APP_REQUEST_TIMEOUT_S);

        let mut results: Vec<(&str, i32)> = Vec::with_capacity(NOTIFICATIONS.len());
        let mut registered: Vec<CFString> = Vec::new();
        for name in NOTIFICATIONS {
            let cf = CFString::new(name);
            let e = AXObserverAddNotification(
                observer.as_CFTypeRef(),
                app.as_CFTypeRef(),
                cf.as_concrete_TypeRef(),
                refcon,
            );
            results.push((name, e));
            if e == KAX_SUCCESS {
                registered.push(cf);
            }
        }
        let teardown = |registered: &[CFString]| {
            for cf in registered {
                AXObserverRemoveNotification(
                    observer.as_CFTypeRef(),
                    app.as_CFTypeRef(),
                    cf.as_concrete_TypeRef(),
                );
            }
        };
        if let Err(why) = registration_outcome(&results) {
            teardown(&registered);
            drop(app);
            drop(observer);
            drop(Arc::from_raw(refcon as *const Notify));
            let _ = ready.send(Err(why));
            return;
        }

        // The observer owns its source; the loop retains it while attached.
        // Holding our own retain keeps it valid across the removal below.
        let source = core_foundation::runloop::CFRunLoopSource::wrap_under_get_rule(
            AXObserverGetRunLoopSource(observer.as_CFTypeRef()),
        );
        let run_loop = CFRunLoop::get_current();
        run_loop.add_source(&source, kCFRunLoopDefaultMode);
        if ready.send(Ok(run_loop.clone())).is_err() {
            // The waiter gave up; nobody will stop us, so do not start.
            stop.store(true, Ordering::SeqCst);
        }

        while !stop.load(Ordering::SeqCst) {
            // `Finished` means the loop has nothing left to watch; spinning
            // on it would burn a core, so leave.
            if let CFRunLoopRunResult::Finished =
                CFRunLoop::run_in_mode(kCFRunLoopDefaultMode, LOOP_SLICE, false)
            {
                break;
            }
        }

        run_loop.remove_source(&source, kCFRunLoopDefaultMode);
        teardown(&registered);
        drop(source);
        drop(app);
        drop(observer);
        drop(Arc::from_raw(refcon as *const Notify));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tests share the live-thread counter and the same app, so they take
    /// turns.
    static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn os_threads() -> usize {
        let out = std::process::Command::new("ps")
            .args(["-M", "-p", &std::process::id().to_string()])
            .output()
            .expect("ps runs");
        // One header row, then one row per thread.
        String::from_utf8_lossy(&out.stdout).lines().count() - 1
    }

    /// Finder is always running and is not driven by anything here; observing
    /// it is passive. `None`, saying why, when Accessibility is not granted.
    fn finder_pid() -> Option<i32> {
        if !crate::permissions().accessibility {
            eprintln!("skipping: Accessibility is not granted to this process");
            return None;
        }
        let out = std::process::Command::new("pgrep")
            .args(["-x", "Finder"])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()?
            .trim()
            .parse()
            .ok()
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    /// Resource hygiene is the point of this module: creating and dropping
    /// observers over and over must leave nothing behind. Every drop joins its
    /// thread, so the count of observer threads is exactly zero afterwards,
    /// and the process holds no more OS threads than it did.
    #[test]
    fn sequential_watches_leave_no_thread_behind() {
        let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
        let Some(pid) = finder_pid() else { return };
        let rt = runtime();
        drop(rt.block_on(watch_pid(pid)));
        let before = os_threads();
        let mut watched = 0;
        for _ in 0..40 {
            let w = rt.block_on(watch_pid(pid));
            watched += w.is_some() as usize;
            if w.is_some() {
                assert_eq!(LIVE_THREADS.load(Ordering::SeqCst), 1);
            }
            drop(w);
            assert_eq!(LIVE_THREADS.load(Ordering::SeqCst), 0);
        }
        assert!(watched > 0, "Finder refused every observation");
        let after = os_threads();
        assert!(
            after <= before,
            "OS threads grew from {before} to {after} over 40 watches"
        );
    }

    /// Dropped at once, and dropped while notifications are being signalled:
    /// both tear down promptly.
    #[test]
    fn a_watch_dropped_at_once_or_mid_burst_still_tears_down() {
        let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
        let Some(pid) = finder_pid() else { return };
        let rt = runtime();
        let started = std::time::Instant::now();
        for _ in 0..20 {
            let w = rt.block_on(watch_pid(pid));
            if let Some(w) = &w {
                w.changed.notify_one();
            }
            drop(w);
        }
        assert_eq!(LIVE_THREADS.load(Ordering::SeqCst), 0);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "20 create/drop cycles took {:?}",
            started.elapsed()
        );
    }

    /// A process that does not exist is not observable: `None`, promptly, and
    /// no thread left over.
    #[test]
    fn a_missing_process_is_refused_without_leaving_a_thread() {
        let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
        let rt = runtime();
        assert!(rt.block_on(watch_pid(0x7fff_fff0)).is_none());
        assert_eq!(LIVE_THREADS.load(Ordering::SeqCst), 0);
    }
}
