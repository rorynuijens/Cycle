//! The made-up road the 3D view draws: the route's climbs and distance, laid
//! out through a world of its own.
//!
//! The road keeps everything the ride is made of and nothing else of the real
//! place:
//!
//! - **Distance.** It is laid out metre by metre from the distance ridden, so
//!   1 km ridden is 1 km of road (stylized-route skill, rule 4).
//! - **Height.** Every point is at the GPX's own height for its distance, and
//!   every GPX point is a point of the road, so the road between them rises
//!   in the same straight lines the physics interpolates (rules 2 and 3).
//! - **The bends the rider brakes for.** Where the ride slows for a bend
//!   ([`bends::curvatures`], read by the SIM physics), the road bends with
//!   the same radius at the same distance, so a hairpin felt is a hairpin
//!   seen.
//!
//! Everything else is made up. The slow wander of the real road and its GPS
//! scatter give way to long, gentle bends, and the road never turns more than
//! [`MAX_HEADING_RAD`] from one way forward — a bend that would take it
//! further is mirrored, or cut short at the same radius. So the road can
//! never come back on itself: an out-and-back, a lap or a U-turn rides on
//! into new country instead of over its own tarmac.
//!
//! Pure: no GTK, no GL. Scene space as in `course3d`: metres, x east, y up,
//! z south; the road sets off north (−z) and the scene is centred on it.

use crate::data::bends;
use crate::data::course3d::RoadPoint;
use crate::data::route::Route;
use crate::data::world::splitmix;

/// The furthest the road turns from straight ahead, in radians (75°). Every
/// metre of road gains at least `cos 75° ≈ 0.26` m forward, so two points of
/// it are at least a quarter of the distance between them apart.
pub const MAX_HEADING_RAD: f32 = 75.0 * std::f32::consts::PI / 180.0;

/// Bends of the route tighter than this radius, in metres, are kept; tighter
/// than [`KEEP_FULL_R_M`], kept whole. At the physics' 55 km/h ceiling and
/// 2 m/s² round a bend, nothing gentler than about 117 m slows the rider.
const KEEP_FROM_R_M: f32 = 250.0;
const KEEP_FULL_R_M: f32 = 150.0;

/// The made-up bends: never tighter than this radius, in metres — a bend
/// nobody brakes for.
pub const GENTLE_R_M: f32 = 400.0;
/// How far off straight ahead the gentle bends swing the road, at most.
const WANDER_RAD: f32 = 28.0 * std::f32::consts::PI / 180.0;
/// How far the road steers to close a gap in its heading: it closes most of
/// it over about this many metres.
const STEER_M: f32 = 120.0;
/// The road steers back towards its own middle line once it has drifted this
/// far to one side, by up to [`HOLD_RAD`] — so a route all of whose bends turn
/// one way still keeps to a strip of country.
const HOLD_M: f32 = 1_500.0;
const HOLD_RAD: f32 = 20.0 * std::f32::consts::PI / 180.0;

/// The road is worked out a metre at a time, and a point is kept every this
/// many metres on a straight, or wherever its heading has turned by
/// [`EMIT_TURN_RAD`] since the last — so a hairpin is drawn round, not as a
/// corner.
const STEP_M: f64 = 1.0;
const EMIT_M: f32 = 6.0;
const EMIT_TURN_RAD: f32 = 3.0 * std::f32::consts::PI / 180.0;

/// The made-up road for `route`. Empty for a route of fewer than two points.
pub fn lay(route: &Route) -> Vec<RoadPoint> {
    let pts = &route.points;
    if pts.len() < 2 {
        return Vec::new();
    }
    let real = bends::curvatures(route);
    let seed = seed_of(route);

    // Where the road is worked out: every metre, and at every GPX point.
    let start = f64::from(pts[0].distance_m);
    let end = f64::from(pts[pts.len() - 1].distance_m);
    let mut at: Vec<(f64, bool)> = Vec::new();
    let mut gpx = pts.iter().map(|p| f64::from(p.distance_m)).peekable();
    let mut d = start;
    while d <= end {
        while let Some(&g) = gpx.peek() {
            if g <= d + 1e-6 {
                if at.last().is_none_or(|&(last, _)| g > last + 1e-6) {
                    at.push((g, true));
                } else if let Some(last) = at.last_mut() {
                    last.1 = true;
                }
                gpx.next();
            } else {
                break;
            }
        }
        if at.last().is_none_or(|&(last, _)| d > last + 1e-6) {
            at.push((d, false));
        }
        d += STEP_M;
    }
    for g in gpx {
        if at.last().is_none_or(|&(last, _)| g > last + 1e-6) {
            at.push((g, true));
        }
    }

    // The route's own bends at each of those distances, and how much of each
    // is kept.
    let kept: Vec<(f32, f32)> = at
        .iter()
        .map(|&(d, _)| {
            let k = curvature_at(route, &real, d as f32);
            let w = smoothstep(1.0 / KEEP_FROM_R_M, 1.0 / KEEP_FULL_R_M, k.abs());
            (k, w)
        })
        .collect();

    let mut out = Vec::with_capacity(at.len() / 4);
    let (mut x, mut z, mut heading) = (0.0f64, 0.0f64, 0.0f32);
    let mut bend = Bend::default();
    let mut emitted: Option<(f32, f32)> = None;
    for i in 0..at.len() {
        let (d, is_gpx) = at[i];
        let d32 = d as f32;
        let last = i + 1 == at.len();
        let due = emitted
            .is_none_or(|(ed, eh)| d32 - ed >= EMIT_M || (heading - eh).abs() >= EMIT_TURN_RAD);
        if is_gpx || last || due {
            out.push(RoadPoint {
                pos: [x as f32, route.elevation_at(d32), z as f32],
                distance_m: d32,
            });
            emitted = Some((d32, heading));
        }
        if last {
            break;
        }
        // A bend starts: which way round it turns, and how much of it is
        // kept, are settled once for the whole bend.
        if kept[i].1 > 0.0 && (i == 0 || kept[i - 1].1 == 0.0) {
            bend = plan_bend(&at, &kept, i, heading);
        }
        let ds = (at[i + 1].0 - d) as f32;
        let (k, w) = kept[i];
        let w = if (bend.from..bend.to).contains(&d) {
            w
        } else {
            0.0
        };
        let free = steer(seed, d32, heading, x as f32);
        let curve = w * bend.mirror * k + (1.0 - w) * free;
        let next = (heading + curve * ds).clamp(-MAX_HEADING_RAD, MAX_HEADING_RAD);
        let mid = f64::from((heading + next) / 2.0);
        x += f64::from(ds) * mid.sin();
        z -= f64::from(ds) * mid.cos();
        heading = next;
    }

    // Centre the scene on the road, so no vertex sits tens of kilometres from
    // the origin.
    let (mut x_lo, mut x_hi, mut z_lo, mut z_hi) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
    for p in &out {
        x_lo = x_lo.min(p.pos[0]);
        x_hi = x_hi.max(p.pos[0]);
        z_lo = z_lo.min(p.pos[2]);
        z_hi = z_hi.max(p.pos[2]);
    }
    let (cx, cz) = ((x_lo + x_hi) / 2.0, (z_lo + z_hi) / 2.0);
    for p in &mut out {
        p.pos[0] -= cx;
        p.pos[2] -= cz;
    }
    out
}

/// How one of the route's bends is laid: `mirror` 1 the route's own way
/// round, −1 the other; and the stretch, `from..to` metres, over which it
/// turns.
#[derive(Debug, Clone, Copy)]
struct Bend {
    mirror: f32,
    from: f64,
    to: f64,
}

impl Default for Bend {
    fn default() -> Self {
        Self {
            mirror: 1.0,
            from: f64::MIN,
            to: f64::MAX,
        }
    }
}

/// Lay the bend starting at `at[i]`, the road heading `heading`.
///
/// It turns whichever way round keeps the road nearer straight ahead — the
/// route's own way on a tie. If even so it would take the road past
/// [`MAX_HEADING_RAD`] — a hairpin, a U-turn — only its middle is kept, at its
/// own radius: as much turning as there is room for, centred on its tightest
/// point, where the rider is slowest.
fn plan_bend(at: &[(f64, bool)], kept: &[(f32, f32)], i: usize, heading: f32) -> Bend {
    let mut turn = 0.0f32;
    let mut gross = vec![0.0f32];
    let (mut hi, mut lo) = (0.0f32, 0.0f32);
    let (mut tightest, mut tightest_at) = (0.0f32, 0usize);
    let mut j = i;
    while j + 1 < at.len() && kept[j].1 > 0.0 {
        let step = kept[j].1 * kept[j].0 * (at[j + 1].0 - at[j].0) as f32;
        turn += step;
        gross.push(gross[gross.len() - 1] + step.abs());
        hi = hi.max(turn);
        lo = lo.min(turn);
        if (kept[j].1 * kept[j].0).abs() > tightest {
            tightest = (kept[j].1 * kept[j].0).abs();
            tightest_at = j - i;
        }
        j += 1;
    }
    let own = (heading + hi).abs().max((heading + lo).abs());
    let mirrored = (heading - lo).abs().max((heading - hi).abs());
    let mirror = if mirrored + 1e-4 < own { -1.0 } else { 1.0 };
    let (hi, lo) = if mirror > 0.0 { (hi, lo) } else { (-lo, -hi) };
    if heading + hi <= MAX_HEADING_RAD && heading + lo >= -MAX_HEADING_RAD {
        return Bend {
            mirror,
            ..Bend::default()
        };
    }
    let room = if heading + hi > MAX_HEADING_RAD {
        MAX_HEADING_RAD - heading
    } else {
        MAX_HEADING_RAD + heading
    }
    .max(0.0);
    let total = gross[gross.len() - 1];
    let centre = gross[tightest_at];
    let from_g = (centre - room / 2.0).clamp(0.0, (total - room).max(0.0));
    let to_g = from_g + room;
    let dist = |g: f32| {
        let k = gross.partition_point(|&x| x < g).min(gross.len() - 1);
        at[i + k].0
    };
    Bend {
        mirror,
        from: dist(from_g),
        to: dist(to_g),
    }
}

/// The made-up bends between the route's own: the curvature that steers the
/// road towards a heading that swings gently from side to side, and back
/// towards its middle line once it drifts far off it — never tighter than
/// [`GENTLE_R_M`].
fn steer(seed: u64, d: f32, heading: f32, x: f32) -> f32 {
    let target = wander(seed, d) - (x / HOLD_M).clamp(-1.0, 1.0) * HOLD_RAD;
    ((target - heading) / STEER_M).clamp(-1.0 / GENTLE_R_M, 1.0 / GENTLE_R_M)
}

/// The heading the gentle bends swing towards at `d`: two slow waves of
/// seeded length and phase.
fn wander(seed: u64, d: f32) -> f32 {
    let unit = |h: u64| (h >> 40) as f32 / (1u64 << 24) as f32;
    let tau = std::f32::consts::TAU;
    let (h1, h2) = (splitmix(seed ^ 0xBE4D), splitmix(seed ^ 0x5A1D));
    let long = 900.0 + 700.0 * unit(h1);
    let short = 380.0 + 260.0 * unit(h2);
    let a = (tau * d / long + tau * unit(h1.rotate_left(17))).sin();
    let b = (tau * d / short + tau * unit(h2.rotate_left(17))).sin();
    WANDER_RAD * (0.65 * a + 0.35 * b)
}

/// The route's own curvature at `d`, between the GPX points either side.
fn curvature_at(route: &Route, real: &[f32], d: f32) -> f32 {
    let pts = &route.points;
    let i = pts.partition_point(|p| p.distance_m <= d);
    if i == 0 {
        return real.first().copied().unwrap_or(0.0);
    }
    if i >= pts.len() {
        return real.last().copied().unwrap_or(0.0);
    }
    let (a, b) = (&pts[i - 1], &pts[i]);
    let span = b.distance_m - a.distance_m;
    let t = if span > 1e-3 {
        (d - a.distance_m) / span
    } else {
        0.0
    };
    let (ka, kb) = (
        real.get(i - 1).copied().unwrap_or(0.0),
        real.get(i).copied().unwrap_or(0.0),
    );
    ka + (kb - ka) * t
}

/// The same route always lays the same road: a seed from its own points.
fn seed_of(route: &Route) -> u64 {
    route.points.iter().fold(0x7EAC_0AD5u64, |h, p| {
        splitmix(h ^ p.lat.to_bits() ^ p.lng.to_bits().rotate_left(29))
    })
}

fn smoothstep(lo: f32, hi: f32, x: f32) -> f32 {
    let t = ((x - lo) / (hi - lo)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::course3d::LocalFrame;
    use crate::data::route::RoutePoint;

    /// A route through `(east, north, elevation)` metres around Monaco.
    fn route(points: &[(f64, f64, f32)]) -> Route {
        let frame = LocalFrame::at(43.73, 7.42);
        let mut distance = 0.0f32;
        let mut out = Vec::new();
        for (i, &(e, n, ele)) in points.iter().enumerate() {
            if i > 0 {
                let (pe, pn, _) = points[i - 1];
                distance += ((e - pe).powi(2) + (n - pn).powi(2)).sqrt() as f32;
            }
            let (lat, lng) = frame.to_geo(e, n);
            out.push(RoutePoint {
                lat,
                lng,
                elevation_m: ele,
                distance_m: distance,
                gradient: 0.0,
            });
        }
        let gain = out
            .windows(2)
            .map(|w| (w[1].elevation_m - w[0].elevation_m).max(0.0))
            .sum();
        Route {
            name: "made".into(),
            total_distance_m: distance,
            total_gain_m: gain,
            points: out,
        }
    }

    /// Straight north `len` metres, a point every 8 m, climbing at `grade`.
    fn straight(len: f64, grade: f32) -> Route {
        let n = (len / 8.0) as usize;
        route(
            &(0..=n)
                .map(|i| (0.0, i as f64 * 8.0, i as f32 * 8.0 * grade))
                .collect::<Vec<_>>(),
        )
    }

    /// Out `len` metres north and straight back down the same road, round a
    /// hairpin of `radius` metres; a point every 8 m.
    fn out_and_back(len: f64, radius: f64) -> Route {
        let mut pts: Vec<(f64, f64, f32)> = (0..=(len / 8.0) as usize)
            .map(|i| (0.0, i as f64 * 8.0, 0.0))
            .collect();
        let steps = (radius * std::f64::consts::PI / 4.0).ceil().max(4.0) as usize;
        for k in 1..steps {
            let t = k as f64 / steps as f64 * std::f64::consts::PI;
            pts.push((radius - radius * t.cos(), len + radius * t.sin(), 0.0));
        }
        for i in 0..=(len / 8.0) as usize {
            pts.push((2.0 * radius, len - i as f64 * 8.0, 0.0));
        }
        route(&pts)
    }

    /// `laps` times round a circle of `radius` metres, a point every 8 m.
    fn laps(radius: f64, laps: usize) -> Route {
        let per = (std::f64::consts::TAU * radius / 8.0) as usize;
        route(
            &(0..=per * laps)
                .map(|k| {
                    let a = k as f64 / per as f64 * std::f64::consts::TAU;
                    (radius * a.sin(), radius - radius * a.cos(), 0.0)
                })
                .collect::<Vec<_>>(),
        )
    }

    /// Switchbacks up a hill: legs of `leg` metres east and west joined by
    /// hairpins of 12 m, climbing 8 %.
    fn switchbacks(legs: usize, leg: f64) -> Route {
        let mut pts = Vec::new();
        let mut d = 0.0f32;
        let r = 12.0;
        for l in 0..legs {
            let n0 = l as f64 * 2.0 * r;
            let (from, to) = if l % 2 == 0 { (0.0, leg) } else { (leg, 0.0) };
            let steps = (leg / 8.0) as usize;
            for i in 0..steps {
                let e = from + (to - from) * i as f64 / steps as f64;
                pts.push((e, n0, d * 0.08));
                d += 8.0;
            }
            let side = if l % 2 == 0 { 1.0 } else { -1.0 };
            for k in 0..8 {
                let t = k as f64 / 8.0 * std::f64::consts::PI - std::f64::consts::FRAC_PI_2;
                pts.push((to + side * r * t.cos(), n0 + r + r * t.sin(), d * 0.08));
                d += (r * std::f64::consts::PI / 8.0) as f32;
            }
        }
        route(&pts)
    }

    fn arc(road: &[RoadPoint]) -> f32 {
        road.windows(2)
            .map(|w| (w[1].pos[0] - w[0].pos[0]).hypot(w[1].pos[2] - w[0].pos[2]))
            .sum()
    }

    fn headings(road: &[RoadPoint]) -> Vec<f32> {
        road.windows(2)
            .map(|w| (w[1].pos[0] - w[0].pos[0]).atan2(-(w[1].pos[2] - w[0].pos[2])))
            .collect()
    }

    fn all() -> Vec<Route> {
        vec![
            straight(6_000.0, 0.05),
            out_and_back(1_000.0, 6.0),
            out_and_back(600.0, 40.0),
            laps(300.0, 4),
            switchbacks(10, 300.0),
        ]
    }

    #[test]
    fn should_lay_the_road_as_long_as_the_ride() {
        for r in all() {
            let road = lay(&r);
            assert_eq!(road.first().map(|p| p.distance_m), Some(0.0));
            assert_eq!(road.last().map(|p| p.distance_m), Some(r.total_distance_m));
            let err = arc(&road) / r.total_distance_m - 1.0;
            assert!(
                err.abs() <= 0.001,
                "drawn {} m of {} m",
                arc(&road),
                r.total_distance_m
            );
            // Point by point too: the road between two points is as long as
            // the ride between them.
            for w in road.windows(2) {
                let gap = (w[1].pos[0] - w[0].pos[0]).hypot(w[1].pos[2] - w[0].pos[2]);
                let ridden = w[1].distance_m - w[0].distance_m;
                assert!(
                    (gap - ridden).abs() <= 0.01 + ridden * 0.002,
                    "{gap} for {ridden}"
                );
            }
        }
    }

    #[test]
    fn should_put_every_point_at_the_rides_own_height() {
        for r in all() {
            let road = lay(&r);
            for p in &road {
                assert_eq!(
                    p.pos[1],
                    r.elevation_at(p.distance_m),
                    "at {} m",
                    p.distance_m
                );
            }
            // Every GPX point is a point of the road, so the road between
            // them climbs in the physics' own straight lines.
            for g in &r.points {
                assert!(
                    road.iter().any(|p| p.distance_m == g.distance_m),
                    "no road point at the GPX point {} m",
                    g.distance_m
                );
            }
        }
    }

    #[test]
    fn should_never_turn_further_than_the_limit_from_straight_ahead() {
        for r in all() {
            for h in headings(&lay(&r)) {
                assert!(h.abs() <= MAX_HEADING_RAD + 0.01, "{h}");
            }
        }
    }

    #[test]
    fn should_never_come_back_past_itself() {
        // An out-and-back on one road, a hairpin of 6 m, four laps, and
        // switchbacks: every pair of points is at least a quarter of the ride
        // between them apart.
        for r in all() {
            let road = lay(&r);
            for (i, p) in road.iter().enumerate().step_by(3) {
                for q in road[i + 1..].iter().step_by(5) {
                    let along = q.distance_m - p.distance_m;
                    let apart = (q.pos[0] - p.pos[0]).hypot(q.pos[2] - p.pos[2]);
                    assert!(
                        apart >= along * 0.25 - 0.5,
                        "{} m and {} m are {apart} m apart",
                        p.distance_m,
                        q.distance_m
                    );
                }
            }
        }
    }

    /// The road's own curvature at `d`, over `half` metres either side.
    fn curvature(road: &[RoadPoint], d: f32, half: f32) -> f32 {
        let pos = |d: f32| crate::data::course3d::position_at(road, d).expect("a road");
        let (a, b, c) = (pos(d - half), pos(d), pos(d + half));
        let h1 = (b[0] - a[0]).atan2(-(b[2] - a[2]));
        let h2 = (c[0] - b[0]).atan2(-(c[2] - b[2]));
        (h2 - h1) / half
    }

    #[test]
    fn should_bend_where_the_rider_brakes_at_the_radius_they_brake_for() {
        // The 6 m hairpin at 1 km: the physics reads its radius over a 40 m
        // chord; the road turns there at that radius.
        let r = out_and_back(1_000.0, 6.0);
        let real = bends::curvatures(&r);
        let (gi, k) = real
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .map(|(i, k)| (i, *k))
            .expect("bends");
        let d = r.points[gi].distance_m;
        let road = lay(&r);
        let seen = curvature(&road, d, 2.0);
        assert!(
            (seen.abs() - k.abs()).abs() <= k.abs() * 0.1,
            "braked for 1/{} m, saw 1/{} m",
            1.0 / k.abs(),
            1.0 / seen.abs()
        );
    }

    #[test]
    fn should_keep_a_bends_own_way_round_when_it_can() {
        // From straight ahead a single right-hand bend stays a right-hand
        // bend.
        let mut pts: Vec<(f64, f64, f32)> = (0..60).map(|i| (0.0, i as f64 * 8.0, 0.0)).collect();
        let r = 30.0;
        for k in 1..12 {
            let t = k as f64 / 12.0 * std::f64::consts::FRAC_PI_2;
            pts.push((r - r * t.cos(), 472.0 + r * t.sin(), 0.0));
        }
        for i in 1..60 {
            pts.push((r + i as f64 * 8.0, 472.0 + r, 0.0));
        }
        let route = route(&pts);
        let road = lay(&route);
        let d = route.points[65].distance_m;
        assert!(
            curvature(&road, d, 3.0) > 1.0 / 60.0,
            "{}",
            curvature(&road, d, 3.0)
        );
    }

    #[test]
    fn should_bend_only_gently_where_the_route_runs_straight() {
        let road = lay(&straight(8_000.0, 0.0));
        let h = headings(&road);
        let mut swung = 0.0f32;
        for (k, w) in road.windows(3).enumerate() {
            let gap = w[2].distance_m - w[0].distance_m;
            let turn = (h[k + 1] - h[k]).abs();
            assert!(
                turn / (gap / 2.0) <= 1.0 / GENTLE_R_M * 1.05,
                "{turn} over {gap} m"
            );
            swung = swung.max(h[k].abs());
        }
        assert!(
            swung > 0.2,
            "the road should bend a little, swung only {swung}"
        );
    }

    #[test]
    fn should_keep_to_a_strip_of_country() {
        // Laps all turning the same way: the road drifts no further than
        // the hold allows plus a bend.
        for r in [laps(200.0, 30), switchbacks(30, 200.0)] {
            let road = lay(&r);
            let lo = road.iter().map(|p| p.pos[0]).fold(f32::MAX, f32::min);
            let hi = road.iter().map(|p| p.pos[0]).fold(f32::MIN, f32::max);
            assert!(hi - lo < 4.0 * HOLD_M, "{} m wide", hi - lo);
        }
    }

    #[test]
    fn should_lay_the_same_road_every_time() {
        for r in all() {
            assert_eq!(lay(&r), lay(&r));
        }
    }

    #[test]
    fn should_lay_nothing_from_fewer_than_two_points() {
        assert!(lay(&route(&[])).is_empty());
        assert!(lay(&route(&[(0.0, 0.0, 5.0)])).is_empty());
        // Two points on top of each other: a road of no length, but a road.
        let road = lay(&route(&[(0.0, 0.0, 5.0), (0.0, 0.0, 6.0)]));
        assert!(road.iter().all(|p| p.pos.iter().all(|v| v.is_finite())));
    }
}
