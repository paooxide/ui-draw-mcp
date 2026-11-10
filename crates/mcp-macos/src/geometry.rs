//! Pure window geometry: pairing the accessibility view of an application's
//! windows with the window server's, and deciding whether another
//! application's window sits on top of one. No OS calls, so it runs in the
//! tests on any host.

use mcp_window::Rect;

/// Frames this close (in points) are the same window. The window server and
/// the accessibility API round differently, and a minimised window's frame can
/// differ by a pixel or two between them.
const SAME_FRAME_TOLERANCE: f64 = 2.0;

pub(crate) fn same_frame(a: &Rect, b: &Rect) -> bool {
    (a.x - b.x).abs() <= SAME_FRAME_TOLERANCE
        && (a.y - b.y).abs() <= SAME_FRAME_TOLERANCE
        && (a.w - b.w).abs() <= SAME_FRAME_TOLERANCE
        && (a.h - b.h).abs() <= SAME_FRAME_TOLERANCE
}

/// For each window-server frame, the index of the accessibility frame that is
/// the same window, if any. A frame is paired once: two windows stacked at the
/// same place pair off in order instead of both taking the first.
pub(crate) fn pair_frames(server: &[Option<Rect>], ax: &[Option<Rect>]) -> Vec<Option<usize>> {
    let mut taken = vec![false; ax.len()];
    server
        .iter()
        .map(|s| {
            let s = s.as_ref()?;
            let hit = ax
                .iter()
                .enumerate()
                .position(|(i, a)| !taken[i] && a.as_ref().is_some_and(|a| same_frame(s, a)))?;
            taken[hit] = true;
            Some(hit)
        })
        .collect()
}

/// One window in front of the one being captured, as the window server lists it.
pub(crate) struct Above {
    pub pid: i32,
    pub owner: String,
    pub bounds: Rect,
}

/// A covered share below this is not worth refusing a capture for: a drop
/// shadow or a one-pixel border of a neighbouring window.
pub(crate) const MAX_COVERED_SHARE: f64 = 0.05;

fn overlap(a: &Rect, b: &Rect) -> f64 {
    let w = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
    let h = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
    if w > 0.0 && h > 0.0 {
        w * h
    } else {
        0.0
    }
}

/// The other applications whose windows cover part of `target`, with the share
/// of it each covers (summed, so overlapping windows of one application can add
/// to more than the true union; the figure is a reason to refuse, not a
/// measurement). Windows of the target's own process are not occluders: a
/// palette or sheet of the same application is that application's pixels.
pub(crate) fn coverage(target: &Rect, target_pid: i32, above: &[Above]) -> Vec<(String, f64)> {
    let area = target.w * target.h;
    if area <= 0.0 {
        return Vec::new();
    }
    let mut by_owner: Vec<(String, f64)> = Vec::new();
    for w in above.iter().filter(|w| w.pid != target_pid) {
        let share = overlap(target, &w.bounds) / area;
        if share <= 0.0 {
            continue;
        }
        match by_owner.iter_mut().find(|(o, _)| *o == w.owner) {
            Some((_, s)) => *s += share,
            None => by_owner.push((w.owner.clone(), share)),
        }
    }
    by_owner
}

/// Why a window cannot be captured by reading the screen, or `None` when it
/// can: its state, or the other applications on top of it. The message says
/// what a capture would have returned instead.
pub(crate) fn capture_obstacle(
    app: &str,
    id: u32,
    state: mcp_window::WindowState,
    covered_by: &[(String, f64)],
) -> Option<String> {
    use mcp_window::WindowState;
    match state {
        WindowState::Minimized => {
            return Some(format!(
                "window {id} of '{app}' is minimised, so it is not drawn anywhere; capturing its \
                 screen area would photograph whatever else is there. Read it with get_ui_tree, \
                 or un-minimise it yourself"
            ))
        }
        WindowState::Offscreen => {
            return Some(format!(
                "window {id} of '{app}' is not on screen (another Space, or the app is hidden), \
                 so it is not drawn anywhere; capturing its screen area would photograph \
                 whatever else is there. Read it with get_ui_tree, or bring it to this Space \
                 yourself"
            ))
        }
        WindowState::Onscreen => {}
    }
    let total: f64 = covered_by.iter().map(|(_, s)| s).sum();
    if total < MAX_COVERED_SHARE {
        return None;
    }
    let mut owners: Vec<&(String, f64)> = covered_by.iter().collect();
    owners.sort_by(|a, b| b.1.total_cmp(&a.1));
    let list: Vec<String> = owners
        .iter()
        .take(3)
        .map(|(o, s)| format!("{o} ({:.0}%)", (s * 100.0).min(100.0)))
        .collect();
    Some(format!(
        "window {id} of '{app}' is covered by {}; the screen holds their pixels there, not this \
         window's, so capturing it would return another application's content. Read it with \
         get_ui_tree, or uncover it yourself",
        list.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_window::WindowState;

    fn r(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn frames_a_pixel_apart_are_one_window_and_frames_far_apart_are_not() {
        assert!(same_frame(
            &r(10.0, 10.0, 400.0, 300.0),
            &r(11.0, 10.0, 399.0, 300.0)
        ));
        assert!(!same_frame(
            &r(10.0, 10.0, 400.0, 300.0),
            &r(10.0, 10.0, 400.0, 340.0)
        ));
    }

    #[test]
    fn two_windows_at_one_frame_pair_off_in_order_not_both_to_the_first() {
        let same = Some(r(0.0, 0.0, 200.0, 100.0));
        let paired = pair_frames(&[same, same], &[same, same]);
        assert_eq!(paired, vec![Some(0), Some(1)]);
        let paired = pair_frames(&[same, same], &[same]);
        assert_eq!(paired, vec![Some(0), None]);
    }

    #[test]
    fn a_frame_with_no_bounds_pairs_with_nothing() {
        assert_eq!(
            pair_frames(&[None], &[Some(r(0.0, 0.0, 9.0, 9.0))]),
            vec![None]
        );
    }

    fn above(pid: i32, owner: &str, b: Rect) -> Above {
        Above {
            pid,
            owner: owner.into(),
            bounds: b,
        }
    }

    #[test]
    fn a_window_of_another_app_over_the_target_is_an_occluder() {
        let target = r(0.0, 0.0, 400.0, 300.0);
        let cover = coverage(
            &target,
            1,
            &[above(2, "Terminal", r(0.0, 0.0, 400.0, 150.0))],
        );
        assert_eq!(cover.len(), 1);
        assert_eq!(cover[0].0, "Terminal");
        assert!((cover[0].1 - 0.5).abs() < 1e-9);
    }

    #[test]
    fn the_targets_own_windows_and_disjoint_windows_do_not_cover_it() {
        let target = r(0.0, 0.0, 400.0, 300.0);
        let cover = coverage(
            &target,
            1,
            &[
                above(1, "TextEdit", r(0.0, 0.0, 400.0, 300.0)),
                above(2, "Terminal", r(500.0, 0.0, 100.0, 100.0)),
            ],
        );
        assert!(cover.is_empty(), "{cover:?}");
    }

    #[test]
    fn windows_of_one_owner_add_up() {
        let target = r(0.0, 0.0, 100.0, 100.0);
        let cover = coverage(
            &target,
            1,
            &[
                above(2, "Mail", r(0.0, 0.0, 100.0, 20.0)),
                above(2, "Mail", r(0.0, 50.0, 100.0, 20.0)),
            ],
        );
        assert_eq!(cover.len(), 1);
        assert!((cover[0].1 - 0.4).abs() < 1e-9);
    }

    #[test]
    fn a_visible_uncovered_window_can_be_captured() {
        assert_eq!(
            capture_obstacle("Notes", 0, WindowState::Onscreen, &[]),
            None
        );
        // A sliver is not a reason to refuse.
        let sliver = [("Dock".to_string(), 0.01)];
        assert_eq!(
            capture_obstacle("Notes", 0, WindowState::Onscreen, &sliver),
            None
        );
    }

    /// The refusal is the point of the check: an agent that is told "covered
    /// by Terminal" stops trusting the picture, one that gets Terminal's
    /// pixels as Notes' window acts on them.
    #[test]
    fn a_covered_window_is_refused_and_says_by_whom() {
        let by = [("Terminal".to_string(), 0.8), ("Mail".to_string(), 0.1)];
        let m = capture_obstacle("Notes", 2, WindowState::Onscreen, &by).unwrap();
        assert!(m.contains("window 2 of 'Notes'"), "{m}");
        assert!(
            m.contains("Terminal (80%)") && m.contains("Mail (10%)"),
            "{m}"
        );
        assert!(m.contains("another application's content"), "{m}");
    }

    #[test]
    fn a_window_that_is_not_drawn_is_refused_whatever_covers_it() {
        let m = capture_obstacle("Notes", 0, WindowState::Minimized, &[]).unwrap();
        assert!(m.contains("minimised") && m.contains("'Notes'"), "{m}");
        let m = capture_obstacle("Notes", 1, WindowState::Offscreen, &[]).unwrap();
        assert!(m.contains("not on screen"), "{m}");
    }
}
