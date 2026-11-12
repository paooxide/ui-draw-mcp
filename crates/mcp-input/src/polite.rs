//! `foreground = "polite"`: borrow the desktop, then give it back.
//!
//! An action that needs the real pointer or keyboard, or to raise an app,
//! first waits until the person has been idle for a while. It reads which app
//! is in front and where the pointer is, acts, and puts both back. If the
//! person reaches for the mouse while it runs, the action stops and the call
//! fails with a retryable error rather than fighting for the pointer.
//!
//! This is courtesy, not containment: nothing here limits what the borrowed
//! app does while it is in front, and a person who is typing and not moving
//! the mouse is only noticed at the moment the call starts (see
//! `docs/threat-model.md`). The pure decisions are free functions; the loan is
//! the only part that touches a backend.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mcp_types::{CancelToken, Envelope, ErrorCode};
use serde_json::{json, Value};

use crate::backend::{DeskState, InputBackend};

/// The two timings of `[input] polite_idle_ms` and `polite_max_wait_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoliteConfig {
    /// How long the person must have been idle before a call borrows.
    pub idle_ms: u64,
    /// The longest a call waits for that before failing.
    pub max_wait_ms: u64,
}

impl Default for PoliteConfig {
    fn default() -> Self {
        PoliteConfig {
            idle_ms: 1_500,
            max_wait_ms: 15_000,
        }
    }
}

/// How often the idle clock is read while waiting.
const POLL: Duration = Duration::from_millis(100);

/// How long after an action returns the desk is watched for an app that is
/// still on its way to the front.
const LATE_ACTIVATION: Duration = Duration::from_millis(300);
/// How long a restored desk must hold before it is believed.
const CONFIRM: Duration = Duration::from_millis(120);

/// How far (points) the pointer may be from where it was and still count as
/// put back: the OS rounds, and a display scale can move a point by one.
pub const POINTER_TOLERANCE_PX: f64 = 3.0;

/// What one reading of the idle clock means for a call that is waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// The person has been idle long enough: borrow now.
    Go,
    /// Not yet; look again.
    Keep,
    /// Out of time: fail with a retryable error and do nothing.
    GiveUp,
}

/// Decide from one idle reading. `idle_ms` is how long the person has been
/// idle, `waited_ms` how long this call has already waited.
pub fn judge_wait(idle_ms: u64, waited_ms: u64, cfg: &PoliteConfig) -> Wait {
    if idle_ms >= cfg.idle_ms {
        Wait::Go
    } else if waited_ms >= cfg.max_wait_ms {
        Wait::GiveUp
    } else {
        Wait::Keep
    }
}

/// Whether the pointer is back where it was, to within a few points. An
/// unknown position on either side is not "moved": nothing was restored, and
/// nothing is claimed.
pub fn pointer_is_back(saved: Option<(f64, f64)>, now: Option<(f64, f64)>) -> Option<bool> {
    let (a, b) = (saved?, now?);
    Some(((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt() <= POINTER_TOLERANCE_PX)
}

/// Whether the app that was in front is in front again.
pub fn app_is_back(saved: &DeskState, now: &DeskState) -> Option<bool> {
    Some(saved.app.as_ref()?.pid == now.app.as_ref()?.pid)
}

/// What the desk looked like after giving it back, as the result reports it.
/// A half is `None` when it was not known or not asked for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Returned {
    pub app: Option<bool>,
    pub pointer: Option<bool>,
}

/// A call that could not borrow the desktop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoliteError {
    /// The person did not go idle within the wait. Retryable.
    Busy { waited_ms: u64, idle_ms: u64 },
    /// This backend cannot say when the person is idle, so politeness cannot
    /// be kept; it refuses rather than act on a guess.
    NoIdleClock,
    /// The kill switch or a cancel arrived while waiting.
    Halted(&'static str),
}

impl PoliteError {
    pub fn envelope(&self, tool: &str) -> Envelope {
        match self {
            PoliteError::Busy { waited_ms, idle_ms } => Envelope::fail_with(
                tool,
                ErrorCode::Timeout,
                format!(
                    "the user is active (idle for {idle_ms} ms after waiting {waited_ms} ms), so \
                     foreground = \"polite\" did not borrow the pointer or keyboard; nothing was \
                     done"
                ),
                "retryable: call again in a few seconds, or do this step through the \
                 accessibility tools (ui_action, set_value) that do not need the desktop",
            ),
            PoliteError::NoIdleClock => Envelope::fail_with(
                tool,
                ErrorCode::UnsupportedOs,
                "foreground = \"polite\" needs to know when the user is idle, and this platform \
                 cannot say, so nothing was done",
                "use foreground = \"never\" (background-safe tools only) or \"allow\"",
            ),
            PoliteError::Halted(why) => Envelope::fail_with(
                tool,
                ErrorCode::Timeout,
                format!("stopped while waiting for the user to go idle: {why}; nothing was done"),
                "clear the cause and call again",
            ),
        }
    }
}

/// The error an action ends with when the person took the pointer during it:
/// retryable, and not the kill switch.
pub fn user_active_error(mut env: Envelope) -> Envelope {
    if let Some(e) = env.error.as_mut() {
        let was = std::mem::take(&mut e.message);
        e.code = ErrorCode::Timeout;
        e.message = format!(
            "the user is active: they took the pointer while this ran, so it stopped and the \
             pointer and keys were released ({was})"
        );
        e.suggestion = Some(
            "retryable: call again when they are done; check what already happened before \
             repeating a step"
                .into(),
        );
    }
    env
}

/// Give the desk back after a call that produced `env`, and say so in it.
///
/// `since` is the backend's takeover count when the call began. The human
/// override watcher cannot trip the kill switch in this mode; it abandons what
/// is in flight and counts a takeover, which is how a person reaching for the
/// mouse reaches here. An action that failed because of it becomes a
/// retryable "user active" error, and the pointer is left with the person.
pub async fn settle(
    loan: Loan,
    backend: &dyn InputBackend,
    since: u64,
    mut env: Envelope,
) -> Envelope {
    let interrupted = backend.takeover_generation() != since;
    if interrupted && !env.ok {
        env = user_active_error(env);
    }
    let waited_ms = loan.waited_ms();
    let returned = loan.give_back(!interrupted).await;
    let report = json!({
        "waited_ms": waited_ms,
        "user_active": interrupted,
        "returned": { "app": returned.app, "pointer": returned.pointer },
    });
    match env.data.as_mut() {
        Some(Value::Object(d)) => {
            d.insert("polite".into(), report);
        }
        None => env.data = Some(json!({ "polite": report })),
        Some(_) => {}
    }
    env
}

/// The borrowed desktop. Giving it back is [`Loan::give_back`]; if the loan is
/// dropped without that (an error path, a cancelled call) the desk is put back
/// anyway, without waiting to see it land.
pub struct Loan {
    backend: Arc<dyn InputBackend>,
    saved: DeskState,
    waited_ms: u64,
    armed: bool,
}

impl Loan {
    /// Wait for the person to be idle (`halted` is asked between readings, so
    /// the kill switch and a cancel end the wait), then record the desk.
    pub async fn borrow(
        backend: Arc<dyn InputBackend>,
        cfg: PoliteConfig,
        halted: impl Fn() -> Option<&'static str>,
    ) -> Result<Loan, PoliteError> {
        let started = tokio::time::Instant::now();
        loop {
            if let Some(why) = halted() {
                return Err(PoliteError::Halted(why));
            }
            let Some(idle) = backend.user_idle_ms().await else {
                return Err(PoliteError::NoIdleClock);
            };
            let waited = started.elapsed().as_millis() as u64;
            match judge_wait(idle, waited, &cfg) {
                Wait::Go => {
                    // Read after the wait, so what is put back is what the
                    // person left when they went idle.
                    let saved = backend.desk_state().await;
                    return Ok(Loan {
                        backend,
                        saved,
                        waited_ms: waited,
                        armed: true,
                    });
                }
                Wait::GiveUp => {
                    return Err(PoliteError::Busy {
                        waited_ms: waited,
                        idle_ms: idle,
                    })
                }
                Wait::Keep => tokio::time::sleep(POLL).await,
            }
        }
    }

    pub fn waited_ms(&self) -> u64 {
        self.waited_ms
    }

    /// Put the desk back and report whether it landed. `pointer` is false when
    /// the person has taken the pointer: warping it away from their hand would
    /// be the opposite of polite, so only the app is put back.
    ///
    /// An app comes forward a beat after it is asked to (measured: `focus_app`
    /// returns before the app is in front), so the desk is not trusted the
    /// moment the action returns. It is watched for a short while for a late
    /// activation, put back, and checked again after a pause, up to three
    /// rounds, because an activation that lands during the restore would
    /// otherwise leave the app in front with the report saying it was not.
    pub async fn give_back(mut self, pointer: bool) -> Returned {
        self.armed = false;
        let mut now = self.backend.desk_state().await;
        let began = tokio::time::Instant::now();
        while began.elapsed() < LATE_ACTIVATION
            && self.saved.app.is_some()
            && app_is_back(&self.saved, &now) == Some(true)
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
            now = self.backend.desk_state().await;
        }
        for _ in 0..3 {
            if self.is_back(&now, pointer) {
                // Confirmed only if it holds a moment later.
                tokio::time::sleep(CONFIRM).await;
                now = self.backend.desk_state().await;
                if self.is_back(&now, pointer) {
                    break;
                }
                continue;
            }
            self.backend.restore_desk(&self.saved, pointer);
            for _ in 0..10 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                now = self.backend.desk_state().await;
                if self.is_back(&now, pointer) {
                    break;
                }
            }
        }
        Returned {
            app: app_is_back(&self.saved, &now),
            pointer: if pointer {
                pointer_is_back(self.saved.pointer, now.pointer)
            } else {
                None
            },
        }
    }

    /// Whether `now` matches the saved desk in the parts asked for. A part
    /// that was not known is not held against it.
    fn is_back(&self, now: &DeskState, pointer: bool) -> bool {
        app_is_back(&self.saved, now).unwrap_or(true)
            && (!pointer || pointer_is_back(self.saved.pointer, now.pointer).unwrap_or(true))
    }
}

impl Drop for Loan {
    fn drop(&mut self) {
        if self.armed {
            self.backend.restore_desk(&self.saved, true);
        }
    }
}

/// Lends the desktop to an engine that has no input backend of its own (the
/// window engine), by the same rules as the input engine's own calls.
pub struct InputDesk {
    backend: Arc<dyn InputBackend>,
    cfg: PoliteConfig,
    halt: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

impl InputDesk {
    /// `halt` says whether the kill switch is engaged; it ends the wait.
    pub fn new(
        backend: Arc<dyn InputBackend>,
        cfg: PoliteConfig,
        halt: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    ) -> Self {
        InputDesk { backend, cfg, halt }
    }
}

struct DeskLoan {
    loan: Loan,
    backend: Arc<dyn InputBackend>,
    since: u64,
}

#[async_trait]
impl mcp_types::Desk for InputDesk {
    async fn borrow(
        &self,
        tool: &str,
        cancel: &CancelToken,
    ) -> Result<Box<dyn mcp_types::Loan>, Envelope> {
        let halt = self.halt.clone();
        let cancel = cancel.clone();
        let loan = Loan::borrow(self.backend.clone(), self.cfg, move || {
            if halt.as_ref().is_some_and(|h| h()) {
                Some("the kill switch is engaged")
            } else if cancel.is_cancelled() {
                Some("the call was cancelled")
            } else {
                None
            }
        })
        .await
        .map_err(|e| e.envelope(tool))?;
        Ok(Box::new(DeskLoan {
            loan,
            since: self.backend.takeover_generation(),
            backend: self.backend.clone(),
        }))
    }
}

#[async_trait]
impl mcp_types::Loan for DeskLoan {
    async fn settle(self: Box<Self>, outcome: Envelope) -> Envelope {
        settle(self.loan, &*self.backend, self.since, outcome).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::AppRef;

    fn cfg() -> PoliteConfig {
        PoliteConfig {
            idle_ms: 1_500,
            max_wait_ms: 15_000,
        }
    }

    #[test]
    fn a_call_goes_once_the_person_has_been_idle_long_enough() {
        assert_eq!(judge_wait(1_500, 0, &cfg()), Wait::Go);
        assert_eq!(judge_wait(60_000, 14_000, &cfg()), Wait::Go);
        assert_eq!(judge_wait(1_499, 0, &cfg()), Wait::Keep);
        assert_eq!(judge_wait(0, 14_999, &cfg()), Wait::Keep);
    }

    #[test]
    fn a_call_gives_up_when_the_wait_is_spent_but_never_before_checking_idle() {
        assert_eq!(judge_wait(0, 15_000, &cfg()), Wait::GiveUp);
        // Idle on the last look wins over the spent clock: do not refuse a
        // desk that is free.
        assert_eq!(judge_wait(2_000, 15_000, &cfg()), Wait::Go);
    }

    #[test]
    fn zero_idle_borrows_at_once() {
        let c = PoliteConfig {
            idle_ms: 0,
            max_wait_ms: 1,
        };
        assert_eq!(judge_wait(0, 0, &c), Wait::Go);
    }

    #[test]
    fn the_pointer_is_back_within_a_few_points_and_unknown_is_not_claimed() {
        assert_eq!(
            pointer_is_back(Some((100.0, 100.0)), Some((102.0, 101.0))),
            Some(true)
        );
        assert_eq!(
            pointer_is_back(Some((100.0, 100.0)), Some((110.0, 100.0))),
            Some(false)
        );
        assert_eq!(pointer_is_back(None, Some((1.0, 1.0))), None);
        assert_eq!(pointer_is_back(Some((1.0, 1.0)), None), None);
    }

    #[test]
    fn the_app_is_back_when_the_same_process_is_in_front() {
        let at = |pid| DeskState {
            app: Some(AppRef {
                pid,
                name: "x".into(),
            }),
            pointer: None,
        };
        assert_eq!(app_is_back(&at(1), &at(1)), Some(true));
        assert_eq!(app_is_back(&at(1), &at(2)), Some(false));
        assert_eq!(app_is_back(&DeskState::default(), &at(2)), None);
    }

    #[test]
    fn a_busy_error_is_retryable_and_says_nothing_was_done() {
        let e = PoliteError::Busy {
            waited_ms: 15_000,
            idle_ms: 200,
        }
        .envelope("mouse_action");
        let err = e.error.unwrap();
        assert_eq!(err.code, ErrorCode::Timeout);
        assert!(err.message.contains("user is active") && err.message.contains("nothing was done"));
        assert!(err.suggestion.unwrap().contains("retryable"));
    }

    #[test]
    fn a_platform_with_no_idle_clock_is_refused_not_guessed() {
        let e = PoliteError::NoIdleClock.envelope("scroll").error.unwrap();
        assert_eq!(e.code, ErrorCode::UnsupportedOs);
        assert!(e.suggestion.unwrap().contains("never"));
    }

    #[test]
    fn a_takeover_becomes_a_retryable_user_active_error_keeping_the_original() {
        let env = Envelope::fail("drag_drop", ErrorCode::ActionFailed, "drag aborted: x");
        let e = user_active_error(env).error.unwrap();
        assert_eq!(e.code, ErrorCode::Timeout);
        assert!(e.message.starts_with("the user is active"));
        assert!(e.message.contains("drag aborted: x"));
        assert!(e.suggestion.unwrap().contains("retryable"));
    }
}
