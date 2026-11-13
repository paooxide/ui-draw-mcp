//! Trackpad-style scrolling: the schedule of events a finger drag produces.
//!
//! A wheel click is one event; a trackpad is a stream. Apps that smooth,
//! rubber-band or hand a scroll to a nested scroller key off the gesture
//! phases (began, changed, ended), and a flick keeps going after the fingers
//! lift (momentum). [`plan`] turns "scroll `dx`, `dy` pixels over so long" into
//! that stream as plain data so the arithmetic is testable on its own; a
//! backend only posts the events it is handed, at the times it is handed them.

/// Largest total scroll on one axis, in pixels. A flick of a few screens is
/// useful; more is a loop in the caller, which can then look at the page.
pub const MAX_DISTANCE_PX: i32 = 10_000;
/// Longest the finger-down part of a gesture may take.
pub const MAX_DURATION_MS: u64 = 3_000;
/// Used when the caller gives no duration: about a quick drag.
pub const DEFAULT_DURATION_MS: u64 = 250;
/// Gap between events, about one display frame.
pub const FRAME_MS: u64 = 16;
/// Most momentum events after the fingers lift.
pub const MAX_MOMENTUM_EVENTS: usize = 60;
/// Most distance momentum may add, in pixels, on top of the request.
pub const MAX_MOMENTUM_PX: i64 = 3_000;
/// Fraction of its speed a momentum frame keeps. Ten frames leave about a third.
const MOMENTUM_DECAY: f64 = 0.9;

/// Where in a gesture an event falls. The macOS values are the public
/// `CGScrollPhase` and `CGMomentumScrollPhase` constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Fingers down; the first movement rides on this event.
    Began,
    Changed,
    /// Fingers up. Carries no movement.
    Ended,
    /// The first coasting event after the fingers lift.
    MomentumBegan,
    MomentumContinue,
    /// The coast has stopped. Carries no movement.
    MomentumEnded,
}

impl Phase {
    pub fn is_momentum(self) -> bool {
        matches!(
            self,
            Self::MomentumBegan | Self::MomentumContinue | Self::MomentumEnded
        )
    }
    /// The phase that closes the gesture this one belongs to.
    pub fn closing(self) -> Self {
        if self.is_momentum() {
            Self::MomentumEnded
        } else {
            Self::Ended
        }
    }
}

/// One event of the stream. `dx` and `dy` are pixels in the direction the view
/// travels through the document: positive `dy` is down, positive `dx` right,
/// the same as the `scroll` tool's `down` and `right`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelScroll {
    pub phase: Phase,
    pub dx: i32,
    pub dy: i32,
    /// When to post it, from the start of the gesture.
    pub at_ms: u64,
}

/// The planned gesture and what it adds up to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub events: Vec<PixelScroll>,
    /// Pixels the finger-down part moves: exactly the request.
    pub body_dx: i64,
    pub body_dy: i64,
    /// Pixels the coast adds on top, zero without momentum.
    pub momentum_dx: i64,
    pub momentum_dy: i64,
}

/// Check the request against the caps and lay out the events.
///
/// The finger-down part moves exactly `dx`, `dy` in total: each event carries
/// the difference of rounded running totals, so rounding never loses a pixel.
/// Without `momentum` the speed eases in and out, as a drag that stops under
/// the finger; with it the speed is steady until release, since a flick
/// lets go while still moving and the coast continues from that speed.
pub fn plan(dx: i32, dy: i32, duration_ms: Option<u64>, momentum: bool) -> Result<Plan, String> {
    if dx == 0 && dy == 0 {
        return Err("dx and dy are both zero, so there is nothing to scroll".into());
    }
    for (name, v) in [("dx", dx), ("dy", dy)] {
        if v.abs() > MAX_DISTANCE_PX {
            return Err(format!(
                "{name} {v} is beyond the {MAX_DISTANCE_PX} px cap for one gesture; scroll in \
                 several calls and look at the page in between"
            ));
        }
    }
    let duration = duration_ms.unwrap_or(DEFAULT_DURATION_MS);
    if duration > MAX_DURATION_MS {
        return Err(format!(
            "duration_ms {duration} is above the {MAX_DURATION_MS} ms cap for one gesture"
        ));
    }
    // At least one moving event, so a zero duration is a single quick flick.
    let frames = (duration / FRAME_MS).max(1);
    let ease = |t: f64| {
        if momentum {
            t
        } else {
            t * t * (3.0 - 2.0 * t)
        }
    };
    let mut events = Vec::with_capacity(frames as usize + 2);
    let (mut sent_x, mut sent_y) = (0i64, 0i64);
    for i in 1..=frames {
        let t = ease(i as f64 / frames as f64);
        let (cx, cy) = (
            (dx as f64 * t).round() as i64,
            (dy as f64 * t).round() as i64,
        );
        events.push(PixelScroll {
            phase: if i == 1 { Phase::Began } else { Phase::Changed },
            dx: (cx - sent_x) as i32,
            dy: (cy - sent_y) as i32,
            at_ms: (i - 1) * duration / frames,
        });
        (sent_x, sent_y) = (cx, cy);
    }
    events.push(PixelScroll {
        phase: Phase::Ended,
        dx: 0,
        dy: 0,
        at_ms: duration,
    });
    let (mut mx, mut my) = (0i64, 0i64);
    if momentum {
        // Release speed in pixels per frame, then a geometric decay. The
        // running total is rounded per event, as above, so slow tails still
        // move a whole pixel at a time instead of vanishing.
        let (vx, vy) = (dx as f64 / frames as f64, dy as f64 / frames as f64);
        let (mut fx, mut fy) = (0.0f64, 0.0f64);
        let mut k = 0usize;
        loop {
            let scale = MOMENTUM_DECAY.powi(k as i32 + 1);
            let (sx, sy) = (vx * scale, vy * scale);
            if sx.abs().max(sy.abs()) < 1.0 || k >= MAX_MOMENTUM_EVENTS - 1 {
                break;
            }
            let (nfx, nfy) = (fx + sx, fy + sy);
            if (nfx.abs().round() as i64).max(nfy.abs().round() as i64) > MAX_MOMENTUM_PX {
                break;
            }
            let (cx, cy) = (nfx.round() as i64, nfy.round() as i64);
            events.push(PixelScroll {
                phase: if k == 0 {
                    Phase::MomentumBegan
                } else {
                    Phase::MomentumContinue
                },
                dx: (cx - mx) as i32,
                dy: (cy - my) as i32,
                at_ms: duration + (k as u64 + 1) * FRAME_MS,
            });
            (fx, fy, mx, my) = (nfx, nfy, cx, cy);
            k += 1;
        }
        if k > 0 {
            events.push(PixelScroll {
                phase: Phase::MomentumEnded,
                dx: 0,
                dy: 0,
                at_ms: duration + (k as u64 + 1) * FRAME_MS,
            });
        }
    }
    Ok(Plan {
        events,
        body_dx: sent_x,
        body_dy: sent_y,
        momentum_dx: mx,
        momentum_dy: my,
    })
}

/// What to say when a pinch, rotate or swipe is asked for. macOS has no public
/// call that synthesises them (the gesture event types are private to the
/// system), and this server does not use private ones; every one of them has
/// a documented route in the app itself.
pub fn gesture_unsupported(gesture: &str) -> Result<String, String> {
    let instead = match gesture {
        "pinch" | "magnify" | "zoom" => {
            "zoom with the app's own command: keyboard_shortcut cmd+plus / cmd+minus, `scroll` \
             with modifiers [\"ctrl\"] (or [\"cmd\"] where the app binds it), or a View menu item"
        }
        "rotate" => "use the app's rotate command from its menu (control_window menu / ui_action on the menu item)",
        "swipe" => {
            "swipe between pages with the app's back / forward shortcut (cmd+[ and cmd+] or \
             cmd+left / cmd+right); swipe between spaces with keyboard_shortcut ctrl+left / ctrl+right"
        }
        other => {
            return Err(format!(
                "unknown gesture '{other}'; expected pinch, rotate or swipe"
            ))
        }
    };
    Ok(format!(
        "the {gesture} gesture cannot be synthesised: the system's gesture events are not public \
         API and agentctl does not use private ones. Instead, {instead}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sums(p: &Plan) -> (i64, i64, i64, i64) {
        let (mut bx, mut by, mut mx, mut my) = (0, 0, 0, 0);
        for e in &p.events {
            if e.phase.is_momentum() {
                mx += e.dx as i64;
                my += e.dy as i64;
            } else {
                bx += e.dx as i64;
                by += e.dy as i64;
            }
        }
        (bx, by, mx, my)
    }

    #[test]
    fn deltas_sum_exactly_to_the_request_for_awkward_numbers() {
        for (dx, dy, ms) in [
            (0, 997, 250),
            (-333, 7, 1000),
            (1, 1, 16),
            (-1, 0, 0),
            (10_000, -10_000, 3000),
        ] {
            for momentum in [false, true] {
                let p = plan(dx, dy, Some(ms), momentum).unwrap();
                let (bx, by, ..) = sums(&p);
                assert_eq!((bx, by), (dx as i64, dy as i64), "{dx} {dy} {ms}");
                assert_eq!((p.body_dx, p.body_dy), (bx, by));
            }
        }
    }

    #[test]
    fn phases_run_began_changed_ended_and_times_never_go_back() {
        let p = plan(0, 500, Some(160), false).unwrap();
        let ph: Vec<_> = p.events.iter().map(|e| e.phase).collect();
        assert_eq!(ph.first(), Some(&Phase::Began));
        assert_eq!(ph.last(), Some(&Phase::Ended));
        assert!(ph[1..ph.len() - 1].iter().all(|p| *p == Phase::Changed));
        assert_eq!(ph.len(), 160 / FRAME_MS as usize + 1);
        assert!(p.events.windows(2).all(|w| w[0].at_ms <= w[1].at_ms));
        assert_eq!(p.events.last().unwrap().at_ms, 160);
        // Ending carries no movement; a finger lifting does not scroll.
        assert_eq!(
            (p.events.last().unwrap().dx, p.events.last().unwrap().dy),
            (0, 0)
        );
        assert_eq!((p.momentum_dx, p.momentum_dy), (0, 0));
    }

    #[test]
    fn a_zero_duration_is_one_quick_flick_not_a_division_by_zero() {
        let p = plan(5, 0, Some(0), false).unwrap();
        assert_eq!(p.events.len(), 2);
        assert_eq!(p.events[0].dx, 5);
    }

    #[test]
    fn momentum_follows_the_lift_in_the_same_direction_and_decays() {
        let p = plan(0, -800, Some(320), true).unwrap();
        let ph: Vec<_> = p.events.iter().map(|e| e.phase).collect();
        let ended = ph.iter().position(|p| *p == Phase::Ended).unwrap();
        assert_eq!(ph[ended + 1], Phase::MomentumBegan);
        assert_eq!(ph.last(), Some(&Phase::MomentumEnded));
        assert!(ph[ended + 2..ph.len() - 1]
            .iter()
            .all(|p| *p == Phase::MomentumContinue));
        let coast: Vec<i32> = p.events[ended + 1..ph.len() - 1]
            .iter()
            .map(|e| e.dy)
            .collect();
        assert!(coast.iter().all(|d| *d <= 0), "same direction: {coast:?}");
        assert!(coast.first().unwrap().abs() >= coast.last().unwrap().abs());
        assert!(p.momentum_dy < 0 && p.momentum_dy.abs() <= MAX_MOMENTUM_PX);
        assert_eq!(p.momentum_dx, 0);
        assert!(p.events.len() <= 20 + 1 + MAX_MOMENTUM_EVENTS + 1);
    }

    #[test]
    fn a_slow_gesture_has_no_coast_and_no_dangling_momentum_end() {
        let p = plan(0, 3, Some(320), true).unwrap();
        assert!(p.events.iter().all(|e| !e.phase.is_momentum()));
    }

    #[test]
    fn momentum_is_bounded_whatever_the_speed() {
        let p = plan(10_000, 10_000, Some(16), true).unwrap();
        assert!(p.momentum_dx.abs() <= MAX_MOMENTUM_PX && p.momentum_dy.abs() <= MAX_MOMENTUM_PX);
        assert!(p.events.len() <= 3 + MAX_MOMENTUM_EVENTS + 1);
    }

    #[test]
    fn the_caps_and_the_empty_request_are_refused_with_what_to_do() {
        assert!(plan(0, 0, None, false).unwrap_err().contains("nothing"));
        assert!(plan(MAX_DISTANCE_PX + 1, 0, None, false)
            .unwrap_err()
            .contains("several calls"));
        assert!(plan(0, -MAX_DISTANCE_PX - 1, None, false).is_err());
        assert!(plan(0, 10, Some(MAX_DURATION_MS + 1), false).is_err());
        assert!(plan(0, 10, Some(MAX_DURATION_MS), false).is_ok());
    }

    #[test]
    fn eased_motion_starts_and_ends_slower_than_its_middle() {
        let p = plan(0, 1000, Some(480), false).unwrap();
        let moving: Vec<i32> = p.events.iter().map(|e| e.dy).collect();
        let mid = moving[moving.len() / 2];
        assert!(
            moving[0] < mid && moving[moving.len() - 2] < mid,
            "{moving:?}"
        );
    }

    #[test]
    fn closing_phase_matches_the_stage_of_the_gesture() {
        assert_eq!(Phase::Changed.closing(), Phase::Ended);
        assert_eq!(Phase::Began.closing(), Phase::Ended);
        assert_eq!(Phase::MomentumContinue.closing(), Phase::MomentumEnded);
    }

    #[test]
    fn each_unsupported_gesture_names_a_working_equivalent() {
        assert!(gesture_unsupported("pinch").unwrap().contains("cmd+plus"));
        assert!(gesture_unsupported("rotate")
            .unwrap()
            .contains("rotate command"));
        let s = gesture_unsupported("swipe").unwrap();
        assert!(s.contains("ctrl+left") && s.contains("cmd+["));
        assert!(gesture_unsupported("shake").is_err());
    }
}
