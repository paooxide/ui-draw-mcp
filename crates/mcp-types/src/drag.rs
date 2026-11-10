//! The pure maths of a drag that passes through waypoints, shared by the
//! browser's `browser_act drag` and the desktop's `drag_drop` so the two tools
//! take the same `path` and spread their moves the same way.

use crate::args::as_f64;

/// Most waypoints one drag's `path` may hold.
pub const DRAG_PATH_MAX: usize = 200;

/// The points of a drag from `from` to `to`: `steps` evenly spaced moves, the
/// last of them exactly on `to`.
pub fn drag_path(from: (f64, f64), to: (f64, f64), steps: usize) -> Vec<(f64, f64)> {
    let n = steps.max(1);
    (1..=n)
        .map(|i| {
            if i == n {
                return to;
            }
            let t = i as f64 / n as f64;
            (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t)
        })
        .collect()
}

/// A drag's `path`: 1 to [`DRAG_PATH_MAX`] waypoints, each `{x, y}` or
/// `[x, y]`, in whatever frame the caller documents.
pub fn parse_drag_path(v: Option<&serde_json::Value>) -> Result<Vec<(f64, f64)>, String> {
    let Some(v) = v.filter(|v| !v.is_null()) else {
        return Ok(Vec::new());
    };
    let Some(list) = v.as_array() else {
        return Err("'path' must be an array of points, each {x, y}".into());
    };
    if list.is_empty() || list.len() > DRAG_PATH_MAX {
        return Err(format!(
            "'path' holds 1 to {DRAG_PATH_MAX} points, got {}",
            list.len()
        ));
    }
    list.iter()
        .enumerate()
        .map(|(i, p)| {
            let pair = match p {
                serde_json::Value::Object(o) => (o.get("x"), o.get("y")),
                serde_json::Value::Array(a) if a.len() == 2 => (a.first(), a.get(1)),
                _ => (None, None),
            };
            match (pair.0.and_then(as_f64), pair.1.and_then(as_f64)) {
                (Some(x), Some(y)) => Ok((x, y)),
                _ => Err(format!(
                    "'path' point {i} must be {{x, y}} with numbers, got {p}"
                )),
            }
        })
        .collect()
}

/// The points of a drag from `from` through each waypoint in turn, the last
/// of them where it releases: `steps` moves in all, spread over the segments
/// by length with at least one per segment, so every waypoint is landed on
/// exactly.
pub fn drag_route(from: (f64, f64), waypoints: &[(f64, f64)], steps: usize) -> Vec<(f64, f64)> {
    if waypoints.is_empty() {
        return Vec::new();
    }
    let mut prev = from;
    let lengths: Vec<f64> = waypoints
        .iter()
        .map(|&w| {
            let d = ((w.0 - prev.0).powi(2) + (w.1 - prev.1).powi(2)).sqrt();
            prev = w;
            d
        })
        .collect();
    let total: f64 = lengths.iter().sum();
    let n = steps.max(waypoints.len());
    let spare = n - waypoints.len();
    // One step per segment, the rest shared by length (largest remainders
    // first so the sum is exactly `n`).
    let mut per: Vec<usize> = vec![1; waypoints.len()];
    if spare > 0 {
        let shares: Vec<f64> = lengths
            .iter()
            .map(|l| {
                if total > 0.0 {
                    l / total * spare as f64
                } else {
                    spare as f64 / waypoints.len() as f64
                }
            })
            .collect();
        let mut given = 0;
        for (p, s) in per.iter_mut().zip(&shares) {
            *p += s.floor() as usize;
            given += s.floor() as usize;
        }
        let mut order: Vec<usize> = (0..waypoints.len()).collect();
        order.sort_by(|a, b| {
            (shares[*b] - shares[*b].floor())
                .partial_cmp(&(shares[*a] - shares[*a].floor()))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for i in order.into_iter().take(spare - given) {
            per[i] += 1;
        }
    }
    let mut out = Vec::with_capacity(n);
    let mut prev = from;
    for (&w, &k) in waypoints.iter().zip(&per) {
        out.extend(drag_path(prev, w, k));
        prev = w;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_drag_path_is_even_and_ends_on_the_target() {
        let p = drag_path((0.0, 0.0), (100.0, 50.0), 10);
        assert_eq!(p.len(), 10);
        assert_eq!(p[0], (10.0, 5.0));
        assert_eq!(*p.last().unwrap(), (100.0, 50.0));
        assert_eq!(drag_path((1.0, 1.0), (1.0, 1.0), 0), vec![(1.0, 1.0)]);
        // A drag between two identical points still has to emit motion: some
        // targets only begin tracking on the first drag event.
        assert_eq!(drag_path((5.0, 5.0), (5.0, 5.0), 4), vec![(5.0, 5.0); 4]);
        // And it advances on every step, so the target sees motion.
        let line = drag_path((0.0, 0.0), (100.0, 50.0), 10);
        assert!(line.windows(2).all(|w| w[1].0 > w[0].0 && w[1].1 > w[0].1));
    }

    #[test]
    fn a_drag_route_lands_on_every_waypoint_in_order() {
        let from = (0.0, 0.0);
        let way = [(100.0, 0.0), (100.0, 100.0), (0.0, 100.0)];
        let route = drag_route(from, &way, 12);
        assert_eq!(route.len(), 12);
        let hits: Vec<usize> = way
            .iter()
            .map(|w| {
                route
                    .iter()
                    .position(|p| p == w)
                    .expect("waypoint on route")
            })
            .collect();
        assert_eq!(
            hits,
            [3, 7, 11],
            "equal segments share the steps: {route:?}"
        );
        assert_eq!(route.last(), Some(&(0.0, 100.0)));
        // Fewer steps than waypoints: one step per waypoint, nothing skipped.
        let short = drag_route(from, &way, 2);
        assert_eq!(short, way.to_vec());
        // A long segment gets more of the steps than a short one.
        let uneven = drag_route(from, &[(10.0, 0.0), (110.0, 0.0)], 12);
        assert_eq!(uneven.len(), 12);
        assert_eq!(uneven.iter().filter(|p| p.0 <= 10.0).count(), 2);
        assert!(drag_route(from, &[], 12).is_empty());
        assert_eq!(
            drag_route(from, &[(5.0, 5.0)], 4),
            drag_path(from, (5.0, 5.0), 4)
        );
    }

    #[test]
    fn a_path_parses_points_in_either_spelling_and_is_bounded() {
        assert_eq!(
            parse_drag_path(Some(&json!([{"x": 1, "y": "2"}, [3, 4]]))),
            Ok(vec![(1.0, 2.0), (3.0, 4.0)])
        );
        assert_eq!(parse_drag_path(None), Ok(Vec::new()));
        assert_eq!(parse_drag_path(Some(&json!(null))), Ok(Vec::new()));
        assert!(parse_drag_path(Some(&json!([]))).is_err());
        assert!(parse_drag_path(Some(&json!([{"x": 1}]))).is_err());
        assert!(parse_drag_path(Some(&json!("1,2"))).is_err());
        let many: Vec<_> = (0..=DRAG_PATH_MAX)
            .map(|i| json!({"x": i, "y": 0}))
            .collect();
        assert!(parse_drag_path(Some(&json!(many))).is_err());
        assert!(parse_drag_path(Some(&json!(many[..DRAG_PATH_MAX]))).is_ok());
    }
}
