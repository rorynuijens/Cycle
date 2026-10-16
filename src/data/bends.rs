//! The route's bends, as the ride measures them.
//!
//! One measurement, read twice: the SIM physics slows the rider for each bend
//! by its radius (`training/route_engine.rs`), and the 3D view lays its
//! made-up road through the same bends (`data/track.rs`), so a hairpin the
//! rider brakes for is a hairpin they see. The measurement is taken from the
//! GPX alone; nothing the view draws ever feeds back into it.
//!
//! Pure: no GTK, no GL.

use crate::data::course3d::{self, LocalFrame, RoadPoint};
use crate::data::route::Route;

/// Half the chord a bend's radius is measured over, in metres. Long enough to
/// ride over the metre or two of scatter in a recorded GPX, which over a short
/// chord would read as a bend on every straight.
pub const CHORD_M: f32 = 20.0;

/// GPX points either side averaged into the line bends are measured on: enough
/// to ride over a metre or two of GPS scatter.
const SMOOTH_POINTS: usize = 3;

/// Past this radius, in metres, the road is straight for every purpose here:
/// the speed ceiling binds long before a bend this gentle would.
const STRAIGHT_R_M: f32 = 10_000.0;

/// How sharply the road turns at each GPX point, in 1/m: one over the bend's
/// radius, positive turning right (clockwise seen from above), negative
/// turning left, zero on a straight. Within a chord of either end the chord is
/// cut short and the end point is unsmoothed, so its GPS scatter would read as
/// a tight bend; those points are straight.
pub fn curvatures(route: &Route) -> Vec<f32> {
    let Some(frame) = LocalFrame::around(route) else {
        return Vec::new();
    };
    let line = smoothed_line(route, &frame);
    let (Some(first), Some(last)) = (line.first(), line.last()) else {
        return Vec::new();
    };
    let (start, end) = (first.distance_m, last.distance_m);
    line.iter()
        .map(|p| {
            let d = p.distance_m;
            if d - CHORD_M < start || d + CHORD_M > end {
                return 0.0;
            }
            let (Some(a), Some(c)) = (
                course3d::position_at(&line, d - CHORD_M),
                course3d::position_at(&line, d + CHORD_M),
            ) else {
                return 0.0;
            };
            curvature(a, p.pos, c)
        })
        .collect()
}

/// The route in plan, smoothed for measuring bends, in the frame's scene
/// space (x east, z south). Heights are the GPX's own; only `x` and `z` are
/// used.
fn smoothed_line(route: &Route, frame: &LocalFrame) -> Vec<RoadPoint> {
    let raw: Vec<(f64, f64)> = route
        .points
        .iter()
        .map(|p| {
            let (e, n) = frame.to_local(p.lat, p.lng);
            (e, -n)
        })
        .collect();
    let len = raw.len();
    (0..len)
        .map(|i| {
            let k = SMOOTH_POINTS.min(i).min(len - 1 - i);
            let window = &raw[i - k..=i + k];
            let n = window.len() as f64;
            let (x, z) = window
                .iter()
                .fold((0.0, 0.0), |(ax, az), (x, z)| (ax + x, az + z));
            RoadPoint {
                pos: [(x / n) as f32, route.points[i].elevation_m, (z / n) as f32],
                distance_m: route.points[i].distance_m,
            }
        })
        .collect()
}

/// Signed curvature (1/m) of the circle through three points on the ground,
/// `a` then `b` then `c` — positive turning right — or 0 when they are as good
/// as in a line, or as good as one point, which a route with no shape to it
/// is.
fn curvature(a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> f32 {
    let side = |p: [f32; 3], q: [f32; 3]| ((p[0] - q[0]).powi(2) + (p[2] - q[2]).powi(2)).sqrt();
    let (ab, bc, ca) = (side(a, b), side(b, c), side(c, a));
    if ab < 1.0 || bc < 1.0 {
        return 0.0;
    }
    // x east, z south: a positive cross product turns from east towards south,
    // clockwise from above — a right turn.
    let cross = (b[0] - a[0]) * (c[2] - a[2]) - (c[0] - a[0]) * (b[2] - a[2]);
    let r = ab * bc * ca / (2.0 * cross.abs());
    if r.is_finite() && r < STRAIGHT_R_M {
        cross.signum() / r
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::route::RoutePoint;

    /// A route from `(east, north)` metres around Istanbul, flat.
    fn route(points: &[(f64, f64)]) -> Route {
        let frame = LocalFrame::at(41.0, 29.0);
        let mut distance = 0.0f32;
        let mut out = Vec::new();
        for (i, &(e, n)) in points.iter().enumerate() {
            if i > 0 {
                let (pe, pn) = points[i - 1];
                distance += ((e - pe).powi(2) + (n - pn).powi(2)).sqrt() as f32;
            }
            let (lat, lng) = frame.to_geo(e, n);
            out.push(RoutePoint {
                lat,
                lng,
                elevation_m: 0.0,
                distance_m: distance,
                gradient: 0.0,
            });
        }
        Route {
            name: "bend".into(),
            total_distance_m: distance,
            total_gain_m: 0.0,
            points: out,
        }
    }

    /// North, then a quarter circle of `radius` turning `right` or left, then
    /// on; a point every 4 m.
    fn quarter(radius: f64, right: bool) -> Route {
        let side = if right { 1.0 } else { -1.0 };
        let mut pts: Vec<(f64, f64)> = (0..50).map(|i| (0.0, i as f64 * 4.0)).collect();
        let approach = 196.0;
        let steps = (radius * std::f64::consts::FRAC_PI_2 / 4.0).ceil() as usize;
        for k in 1..=steps {
            let t = k as f64 / steps as f64 * std::f64::consts::FRAC_PI_2;
            pts.push((
                side * (radius - radius * t.cos()),
                approach + radius * t.sin(),
            ));
        }
        for i in 1..50 {
            pts.push((side * (radius + i as f64 * 4.0), approach + radius));
        }
        route(&pts)
    }

    fn peak(k: &[f32]) -> f32 {
        k.iter()
            .copied()
            .fold(0.0, |a, b| if b.abs() > a.abs() { b } else { a })
    }

    #[test]
    fn should_measure_a_bend_at_its_own_radius() {
        // Over a 40 m chord the radius reads a little wide of the true one in
        // a short bend; never tighter.
        for r in [15.0, 30.0, 60.0] {
            let k = peak(&curvatures(&quarter(r, true)));
            let measured = 1.0 / k;
            assert!(
                measured >= r as f32 * 0.95 && measured <= r as f32 * 1.6,
                "a {r} m bend measured {measured} m"
            );
        }
    }

    #[test]
    fn should_sign_a_right_turn_positive_and_a_left_turn_negative() {
        assert!(peak(&curvatures(&quarter(30.0, true))) > 0.0);
        assert!(peak(&curvatures(&quarter(30.0, false))) < 0.0);
    }

    #[test]
    fn should_read_a_straight_road_as_straight_end_to_end() {
        let pts: Vec<(f64, f64)> = (0..200).map(|i| (i as f64 * 0.3, i as f64 * 8.0)).collect();
        assert!(curvatures(&route(&pts)).iter().all(|&k| k == 0.0));
    }

    #[test]
    fn should_give_one_curvature_per_point_and_none_for_no_route() {
        let r = quarter(20.0, false);
        assert_eq!(curvatures(&r).len(), r.points.len());
        assert!(curvatures(&route(&[])).is_empty());
        assert!(curvatures(&route(&[(0.0, 0.0)])).iter().all(|&k| k == 0.0));
        let same = route(&[(0.0, 0.0); 40]);
        assert!(curvatures(&same).iter().all(|&k| k == 0.0));
    }
}
