//! Stopping when the human takes the wheel.
//!
//! The most natural way to interrupt something driving your computer is to
//! reach for the mouse. Everyone does it already, without being told, and it
//! needs no key combination the agent might be holding down. A STOP file is a
//! good brake but you have to think of it first; a hand on the mouse is
//! reflexive.
//!
//! So: the backend records every pointer position it *sets*, a watcher samples
//! where the pointer actually *is*, and a persistent divergence between the two
//! means somebody else moved it. That trips the kill switch.
//!
//! **Keyboard is deliberately out of scope.** There is no equivalent signal:
//! detecting human typing needs a CGEventTap, which needs its own TCC grant and
//! a run loop, and would mean the server watching every keystroke on the
//! machine — a worse trade than the feature is worth. Synthetic events also
//! reset the system idle timer, so idle time cannot stand in for it either.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

/// A pointer position the server itself set.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SetPoint {
    pub x: f64,
    pub y: f64,
    pub at_ms: u64,
}

/// How the watcher behaves.
#[derive(Debug, Clone, Copy)]
pub struct OverrideConfig {
    pub enabled: bool,
    /// How far the pointer must be from anywhere the server put it.
    pub threshold_px: f64,
    /// How long after an action the server is still considered to be driving.
    /// Without this, a divergence between two calls would be missed entirely.
    pub grace_ms: u64,
    pub poll_ms: u64,
    /// Consecutive diverging samples before tripping. One is not enough: a
    /// sample can land between the server deciding to move and the OS applying
    /// it, and a false trip is a stopped agent for no reason.
    pub confirm_samples: u8,
    /// How long a set point stays a valid explanation for where the pointer is.
    /// Long enough to cover a drag's intermediate steps and the OS's own
    /// latency.
    pub set_window_ms: u64,
}

impl Default for OverrideConfig {
    fn default() -> Self {
        OverrideConfig {
            enabled: true,
            threshold_px: 12.0,
            grace_ms: 1_500,
            poll_ms: 50,
            confirm_samples: 2,
            set_window_ms: 1_000,
        }
    }
}

/// Whether the server is currently driving the machine.
///
/// Held by the input engine, read by the watcher, so the pointer is only
/// sampled when a divergence would actually mean something. A human moving
/// their own mouse while no agent is acting is not an event.
#[derive(Default)]
pub struct Activity {
    in_flight: AtomicUsize,
    last_end_ms: AtomicU64,
}

/// Marks an input call as in flight for as long as it lives.
pub struct ActivityGuard(Arc<Activity>);

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.0
            .last_end_ms
            .store(mcp_policy::now_ms() as u64, Ordering::SeqCst);
    }
}

impl Activity {
    pub fn new() -> Arc<Self> {
        Arc::new(Activity::default())
    }

    pub fn begin(self: &Arc<Self>) -> ActivityGuard {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        ActivityGuard(self.clone())
    }

    /// True while a call is in flight, or within the grace period after one.
    pub fn driving(&self, now_ms: u64, grace_ms: u64) -> bool {
        if self.in_flight.load(Ordering::SeqCst) > 0 {
            return true;
        }
        let last = self.last_end_ms.load(Ordering::SeqCst);
        last != 0 && now_ms.saturating_sub(last) <= grace_ms
    }
}

/// What a sample means.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Verdict {
    /// Not driving; nothing to judge.
    Idle,
    /// The pointer is where the server put it.
    Consistent,
    /// Diverging, but not yet confirmed.
    Suspicious,
    /// A human has the mouse.
    Tripped {
        observed: (f64, f64),
        nearest: Option<(f64, f64)>,
        distance: f64,
    },
}

/// Decides, from a series of samples, whether somebody else is moving the
/// pointer. Pure: no clock, no backend, so every interesting case is testable.
#[derive(Default)]
pub struct Detector {
    /// Where the pointer was when the server started driving. Until the server
    /// actually moves it, "still where the human left it" is the expected
    /// state, not evidence of interference.
    baseline: Option<(f64, f64)>,
    /// Set once a sample lands on a point the server set. From then on the
    /// baseline is no longer an explanation — which is what lets "the human
    /// moved it back to where it started" trip.
    landed: bool,
    strikes: u8,
}

fn dist(a: (f64, f64), b: (f64, f64)) -> f64 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

impl Detector {
    pub fn new() -> Self {
        Detector::default()
    }

    /// Judge one sample.
    ///
    /// `recent` is the server's own recent pointer writes, newest last.
    /// `observed` is where the pointer actually is; `None` (no sensor, or a
    /// failed read) is never grounds for tripping — an unreadable sensor is not
    /// evidence of a human.
    pub fn observe(
        &mut self,
        now_ms: u64,
        driving: bool,
        recent: &[SetPoint],
        observed: Option<(f64, f64)>,
        cfg: &OverrideConfig,
    ) -> Verdict {
        if !driving {
            *self = Detector::default();
            return Verdict::Idle;
        }
        let Some(observed) = observed else {
            return Verdict::Consistent;
        };
        if self.baseline.is_none() {
            self.baseline = Some(observed);
        }

        // Anywhere the server recently put the pointer explains where it is.
        // The newest set point counts regardless of age: after a single move
        // the pointer simply stays there, possibly for a long time.
        let mut refs: Vec<(f64, f64)> = Vec::new();
        if let Some(last) = recent.last() {
            refs.push((last.x, last.y));
        }
        for p in recent {
            if now_ms.saturating_sub(p.at_ms) <= cfg.set_window_ms {
                refs.push((p.x, p.y));
            }
        }
        let on_a_set_point = refs.iter().any(|r| dist(observed, *r) <= cfg.threshold_px);
        if on_a_set_point {
            self.landed = true;
        }
        if !self.landed {
            if let Some(b) = self.baseline {
                refs.push(b);
            }
        }

        let nearest = refs.iter().copied().min_by(|a, b| {
            dist(observed, *a)
                .partial_cmp(&dist(observed, *b))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let distance = nearest.map(|n| dist(observed, n)).unwrap_or(0.0);

        if nearest.is_none() || distance <= cfg.threshold_px {
            self.strikes = 0;
            return Verdict::Consistent;
        }
        self.strikes = self.strikes.saturating_add(1);
        if self.strikes >= cfg.confirm_samples {
            Verdict::Tripped {
                observed,
                nearest,
                distance,
            }
        } else {
            Verdict::Suspicious
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> OverrideConfig {
        OverrideConfig::default()
    }

    fn sp(x: f64, y: f64, at_ms: u64) -> SetPoint {
        SetPoint { x, y, at_ms }
    }

    #[test]
    fn nothing_is_judged_while_the_server_is_not_driving() {
        let mut d = Detector::new();
        assert_eq!(
            d.observe(1000, false, &[], Some((900.0, 900.0)), &cfg()),
            Verdict::Idle
        );
    }

    /// Two consecutive diverging samples, then a stop. One is not enough: a
    /// sample can land between the decision to move and the OS applying it.
    #[test]
    fn a_sustained_divergence_trips_on_the_second_sample() {
        let mut d = Detector::new();
        let recent = [sp(100.0, 100.0, 1000)];
        assert_eq!(
            d.observe(1010, true, &recent, Some((100.0, 100.0)), &cfg()),
            Verdict::Consistent
        );
        assert_eq!(
            d.observe(1060, true, &recent, Some((400.0, 400.0)), &cfg()),
            Verdict::Suspicious
        );
        assert!(matches!(
            d.observe(1110, true, &recent, Some((420.0, 410.0)), &cfg()),
            Verdict::Tripped { .. }
        ));
    }

    /// Small movement is not a human taking over: pointer coordinates are not
    /// exact, and a threshold that trips on a few pixels stops the agent for
    /// nothing.
    #[test]
    fn small_jitter_is_tolerated() {
        let mut d = Detector::new();
        let recent = [sp(100.0, 100.0, 1000)];
        for t in 0..10 {
            let v = d.observe(1000 + t * 50, true, &recent, Some((105.0, 103.0)), &cfg());
            assert_eq!(v, Verdict::Consistent, "11px away is within tolerance");
        }
    }

    /// A drag writes many points quickly. A sample taken mid-drag lands on an
    /// intermediate position, which is the server's own doing.
    #[test]
    fn a_drag_in_progress_is_consistent() {
        let mut d = Detector::new();
        let recent: Vec<SetPoint> = (0..10)
            .map(|i| sp(100.0 + i as f64 * 20.0, 100.0, 1000 + i as u64 * 10))
            .collect();
        // Observed at an earlier step of the same drag.
        let v = d.observe(1100, true, &recent, Some((160.0, 100.0)), &cfg());
        assert_eq!(v, Verdict::Consistent);
    }

    /// Before the server has moved the pointer, wherever it already was is the
    /// expected place — that is the human's own last position, not evidence of
    /// interference.
    #[test]
    fn the_starting_position_is_not_a_divergence() {
        let mut d = Detector::new();
        for t in 0..5 {
            assert_eq!(
                d.observe(1000 + t * 50, true, &[], Some((800.0, 600.0)), &cfg()),
                Verdict::Consistent
            );
        }
    }

    /// Once the server has actually moved the pointer, the starting position
    /// stops being an explanation — otherwise "the human moved it back" would
    /// be invisible.
    #[test]
    fn returning_to_the_starting_position_trips_once_the_server_has_moved_it() {
        let mut d = Detector::new();
        let start = Some((800.0, 600.0));
        assert_eq!(
            d.observe(1000, true, &[], start, &cfg()),
            Verdict::Consistent
        );
        let recent = [sp(100.0, 100.0, 1050)];
        assert_eq!(
            d.observe(1060, true, &recent, Some((100.0, 100.0)), &cfg()),
            Verdict::Consistent
        );
        // Back where it started, which the server did not do.
        assert_eq!(
            d.observe(1110, true, &recent, start, &cfg()),
            Verdict::Suspicious
        );
        assert!(matches!(
            d.observe(1160, true, &recent, start, &cfg()),
            Verdict::Tripped { .. }
        ));
    }

    /// An unreadable sensor is not evidence of a human. Failing closed here
    /// would mean any backend without a pointer read stops itself constantly.
    #[test]
    fn an_unreadable_pointer_never_trips() {
        let mut d = Detector::new();
        for t in 0..20 {
            assert_eq!(
                d.observe(1000 + t * 50, true, &[], None, &cfg()),
                Verdict::Consistent
            );
        }
    }

    /// Going idle clears the state.
    ///
    /// Both halves matter. The strike count resets, so grievances do not
    /// accumulate across bursts. And so does the baseline: between bursts the
    /// human is free to move their own mouse, so wherever the pointer is when
    /// the server starts again is simply where it is.
    #[test]
    fn going_idle_resets_the_detector() {
        let mut d = Detector::new();
        let recent = [sp(100.0, 100.0, 1000)];
        // Establish a baseline, then land on a point the server set.
        d.observe(1000, true, &recent, Some((100.0, 100.0)), &cfg());
        // Now a divergence counts.
        assert_eq!(
            d.observe(1050, true, &recent, Some((900.0, 900.0)), &cfg()),
            Verdict::Suspicious,
            "one strike accumulated"
        );

        assert_eq!(d.observe(1100, false, &[], None, &cfg()), Verdict::Idle);

        // Re-baselined: the pointer being at (900,900) is now simply where it
        // is, not a second strike carried over from before.
        assert_eq!(
            d.observe(1150, true, &recent, Some((900.0, 900.0)), &cfg()),
            Verdict::Consistent
        );
    }

    /// The very first sample of a burst can never trip: it is what establishes
    /// where the pointer was to begin with.
    #[test]
    fn the_first_sample_of_a_burst_only_establishes_the_baseline() {
        let mut d = Detector::new();
        let recent = [sp(0.0, 0.0, 1000)];
        assert_eq!(
            d.observe(1000, true, &recent, Some((1500.0, 900.0)), &cfg()),
            Verdict::Consistent
        );
    }

    #[test]
    fn activity_covers_a_call_and_its_grace_period() {
        let a = Activity::new();
        assert!(!a.driving(10_000, 1_500));
        {
            let _g = a.begin();
            assert!(a.driving(10_000, 1_500), "in flight");
        }
        let now = mcp_policy::now_ms() as u64;
        assert!(
            a.driving(now, 1_500),
            "still driving during the grace period"
        );
        assert!(!a.driving(now + 5_000, 1_500), "and not after it");
    }

    #[test]
    fn nested_calls_keep_the_session_driving_until_the_last_one_ends() {
        let a = Activity::new();
        let g1 = a.begin();
        let g2 = a.begin();
        drop(g1);
        assert!(a.driving(10_000, 0), "one call is still in flight");
        drop(g2);
        assert!(!a.driving(u64::MAX, 0));
    }
}
