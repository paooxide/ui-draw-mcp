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

/// A covered share below this is not worth warning about: a drop
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
/// to more than the true union; the figure is a reason to warn, not a
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

/// Where the windows covering `target` are, grouped by owner, clipped to the
/// target: the parts of its screen area whose pixels are another application's.
/// Windows of the target's own process are not occluders, as in [`coverage`].
pub(crate) fn occluders(
    target: &Rect,
    target_pid: i32,
    above: &[Above],
) -> Vec<(String, mcp_vision::marks::Rect)> {
    above
        .iter()
        .filter(|w| w.pid != target_pid && overlap(target, &w.bounds) > 0.0)
        .map(|w| {
            let x = target.x.max(w.bounds.x);
            let y = target.y.max(w.bounds.y);
            let right = (target.x + target.w).min(w.bounds.x + w.bounds.w);
            let bottom = (target.y + target.h).min(w.bounds.y + w.bounds.h);
            (
                w.owner.clone(),
                mcp_vision::marks::Rect::new(x, y, right - x, bottom - y),
            )
        })
        .collect()
}

/// Give each covering application the rectangles of its windows.
pub(crate) fn attach_rects(
    owners: &mut [mcp_vision::Covering],
    rects: &[(String, mcp_vision::marks::Rect)],
) {
    for o in owners {
        o.rects = rects
            .iter()
            .filter(|(app, _)| *app == o.app)
            .map(|(_, r)| *r)
            .collect();
    }
}

/// Why a window has no pixels to read off the screen, or `None` when it is
/// drawn. A minimised or off-screen window is refused: its screen area holds
/// whatever else is there, and nothing of the window. Another application
/// covering it is not a refusal; see [`cover_warning`].
pub(crate) fn capture_obstacle(
    app: &str,
    id: u32,
    state: mcp_window::WindowState,
) -> Option<String> {
    use mcp_window::WindowState;
    match state {
        WindowState::Minimized => Some(format!(
            "window {id} of '{app}' is minimised, so it is not drawn anywhere; capturing its \
             screen area would photograph whatever else is there. Read it with get_ui_tree, \
             or un-minimise it yourself"
        )),
        WindowState::Offscreen => Some(format!(
            "window {id} of '{app}' is not on screen (another Space, or the app is hidden), \
             so it is not drawn anywhere; capturing its screen area would photograph \
             whatever else is there. Read it with get_ui_tree, or bring it to this Space \
             yourself"
        )),
        WindowState::Onscreen => None,
    }
}

/// The cover warning an image earns given how it was made. ScreenCaptureKit
/// renders the window itself, so what covers it is not in the picture and
/// there is nothing to warn about; the `screencapture` rectangle grab (and
/// OCR, which reads the screen) shows the covering windows' pixels.
pub(crate) fn cover_warning_for(
    via: &str,
    app: &str,
    id: u32,
    covered_by: &[(String, f64)],
) -> Option<(Vec<mcp_vision::Covering>, String)> {
    if via == "screencapturekit" {
        return None;
    }
    cover_warning(app, id, covered_by)
}

/// The warning for an image of the screen area of a window that other
/// applications cover: who covers it and how much, and that those pixels are
/// theirs. `None` below [`MAX_COVERED_SHARE`], which is a shadow or a border.
/// Largest share first; fractions are capped at 1 and rounded to two places.
pub(crate) fn cover_warning(
    app: &str,
    id: u32,
    covered_by: &[(String, f64)],
) -> Option<(Vec<mcp_vision::Covering>, String)> {
    let total: f64 = covered_by.iter().map(|(_, s)| s).sum();
    if total < MAX_COVERED_SHARE {
        return None;
    }
    let mut owners: Vec<mcp_vision::Covering> = covered_by
        .iter()
        .map(|(o, s)| mcp_vision::Covering {
            app: o.clone(),
            fraction: (s.min(1.0) * 100.0).round() / 100.0,
            rects: Vec::new(),
        })
        .collect();
    owners.sort_by(|a, b| b.fraction.total_cmp(&a.fraction));
    let list: Vec<String> = owners
        .iter()
        .take(3)
        .map(|c| format!("{} ({:.0}%)", c.app, c.fraction * 100.0))
        .collect();
    let msg = format!(
        "window {id} of '{app}' is covered by {}; in those parts the image shows their pixels, \
         not this window's, so do not read them as '{app}' content. Read the window with \
         get_ui_tree, or uncover it yourself and capture again",
        list.join(", ")
    );
    Some((owners, msg))
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
    fn a_drawn_window_has_no_obstacle_and_a_sliver_no_warning() {
        assert_eq!(capture_obstacle("Notes", 0, WindowState::Onscreen), None);
        assert_eq!(cover_warning("Notes", 0, &[]), None);
        // A shadow or a border is not worth a warning.
        let sliver = [("Dock".to_string(), 0.01)];
        assert_eq!(cover_warning("Notes", 0, &sliver), None);
    }

    /// A covered window is captured, and the warning is what stops an agent
    /// acting on Terminal's pixels as if they were Notes'.
    #[test]
    fn a_covered_window_warns_and_says_by_whom() {
        let by = [("Mail".to_string(), 0.1), ("Terminal".to_string(), 0.8)];
        let (owners, m) = cover_warning("Notes", 2, &by).unwrap();
        assert_eq!(
            owners,
            vec![
                mcp_vision::Covering {
                    app: "Terminal".into(),
                    fraction: 0.8,
                    rects: Vec::new(),
                },
                mcp_vision::Covering {
                    app: "Mail".into(),
                    fraction: 0.1,
                    rects: Vec::new(),
                },
            ],
            "largest first"
        );
        assert!(m.contains("window 2 of 'Notes'"), "{m}");
        assert!(
            m.contains("Terminal (80%)") && m.contains("Mail (10%)"),
            "{m}"
        );
        assert!(m.contains("not this window's"), "{m}");
    }

    /// The pixels of a ScreenCaptureKit image are the window's own, so a
    /// warning there would tell the agent to distrust a correct picture; the
    /// rectangle grab's would be missing the one thing that makes it unsafe.
    #[test]
    fn covering_windows_are_clipped_to_the_target_and_given_to_their_owner() {
        let target = Rect {
            x: 100.0,
            y: 100.0,
            w: 400.0,
            h: 300.0,
        };
        let above = [
            Above {
                pid: 7,
                owner: "Terminal".into(),
                bounds: Rect {
                    x: 300.0,
                    y: 50.0,
                    w: 600.0,
                    h: 200.0,
                },
            },
            Above {
                pid: 9,
                owner: "Mail".into(),
                bounds: Rect {
                    x: 900.0,
                    y: 900.0,
                    w: 10.0,
                    h: 10.0,
                },
            },
            // The target's own sheet is not an occluder.
            Above {
                pid: 1,
                owner: "Notes".into(),
                bounds: Rect {
                    x: 120.0,
                    y: 120.0,
                    w: 50.0,
                    h: 50.0,
                },
            },
        ];
        let rects = occluders(&target, 1, &above);
        assert_eq!(
            rects,
            vec![(
                "Terminal".to_string(),
                mcp_vision::marks::Rect::new(300.0, 100.0, 200.0, 150.0)
            )]
        );
        let by = coverage(&target, 1, &above);
        let (mut owners, _) = cover_warning("Notes", 0, &[("Terminal".into(), 0.5)]).unwrap();
        attach_rects(&mut owners, &rects);
        assert_eq!(owners[0].rects.len(), 1);
        assert_eq!(by.len(), 1);
    }

    #[test]
    fn only_a_screen_grab_carries_the_cover_warning() {
        let by = [("Terminal".to_string(), 0.8)];
        assert_eq!(cover_warning_for("screencapturekit", "Notes", 0, &by), None);
        let (owners, _) = cover_warning_for("screencapture", "Notes", 0, &by).unwrap();
        assert_eq!(owners[0].app, "Terminal");
        // Nothing covering: no warning on any route.
        assert_eq!(cover_warning_for("screencapture", "Notes", 0, &[]), None);
    }

    #[test]
    fn overlapping_covers_cannot_report_more_than_all_of_the_window() {
        let by = [("Terminal".to_string(), 1.4)];
        let (owners, m) = cover_warning("Notes", 0, &by).unwrap();
        assert_eq!(owners[0].fraction, 1.0);
        assert!(m.contains("Terminal (100%)"), "{m}");
    }

    #[test]
    fn a_window_that_is_not_drawn_is_refused() {
        let m = capture_obstacle("Notes", 0, WindowState::Minimized).unwrap();
        assert!(m.contains("minimised") && m.contains("'Notes'"), "{m}");
        let m = capture_obstacle("Notes", 1, WindowState::Offscreen).unwrap();
        assert!(m.contains("not on screen"), "{m}");
    }
}
