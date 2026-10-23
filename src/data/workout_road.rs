//! The road a workout is ridden along in the 3D view: made up from the
//! workout itself, so the road climbs through the hard intervals and levels
//! out in the recoveries.
//!
//! The road is laid out so that each stretch is exactly as long as its
//! interval takes at the interval's target power on that stretch's gradient
//! ([`Route::speed_from_power`]). The rider is then placed on it by the
//! workout clock, not by their own watts: a climb starts when the interval
//! does and ends when it ends, whatever the legs are doing, and Skip simply
//! moves the rider on to the next climb.
//!
//! It is scenery and nothing else. The distance never reaches the session,
//! the FIT file, Garmin or the coach, and the trainer is still driven by the
//! workout's targets in ERG — never by this gradient.
//!
//! Pure: no GTK, no GL.

use crate::data::athlete::AthleteProfile;
use crate::data::route::{Route, RoutePoint};
use crate::data::workout::Workout;

/// At or below this share of FTP, in percent, the road is flat.
pub const FLAT_TO_PCT: f32 = 60.0;
/// At or above this share of FTP, in percent, the road is at its steepest.
pub const STEEPEST_FROM_PCT: f32 = 120.0;
/// The steepest the road gets, as a fraction (9 %).
pub const STEEPEST: f32 = 0.09;

/// A ramp is laid in pieces this long, in seconds, each at the target of its
/// middle — so a warm-up is a road that steepens bit by bit.
const RAMP_PIECE_SECS: u32 = 15;
/// The road's points are this far apart, in metres.
const POINT_SPACING_M: f32 = 20.0;
/// Each change of gradient is eased over about this distance either side, in
/// metres, so the road bends up into a climb rather than kinking.
const EASE_M: f32 = 60.0;
/// Slowest the made-up road is ridden, in m/s (7.2 km/h): a stretch at zero
/// target still has some length, so its time is still on the road.
const MIN_SPEED_MS: f32 = 2.0;
/// Where the road sets off. Only its latitude matters to the world — the
/// snow line is worked out from it — and this one is temperate.
const START_LAT: f64 = 47.0;
const START_LNG: f64 = 8.0;
/// Height of the road's start, in metres.
const START_ELEVATION_M: f32 = 200.0;
/// The sphere `route::haversine` measures on, so the road's own distances
/// and its points' spacing agree.
const EARTH_RADIUS_M: f64 = 6_371_000.0;

/// How steep the road is for riding at `pct_ftp` percent of FTP: flat up to
/// [`FLAT_TO_PCT`], rising evenly to [`STEEPEST`] at [`STEEPEST_FROM_PCT`].
pub fn gradient_for(pct_ftp: f32) -> f32 {
    if !pct_ftp.is_finite() {
        return 0.0;
    }
    let t = (pct_ftp - FLAT_TO_PCT) / (STEEPEST_FROM_PCT - FLAT_TO_PCT);
    t.clamp(0.0, 1.0) * STEEPEST
}

/// One stretch of the road: ridden from `start_secs` for `secs` seconds at
/// `speed_ms`, from `start_m` along the road, at `gradient`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Piece {
    start_secs: f32,
    secs: f32,
    start_m: f32,
    speed_ms: f32,
    gradient: f32,
}

/// A workout's road, and where the workout clock puts the rider on it.
#[derive(Debug, Clone)]
pub struct WorkoutRoad {
    pub route: Route,
    pieces: Vec<Piece>,
}

impl WorkoutRoad {
    /// The road for `workout` ridden by `athlete`, or `None` for a workout
    /// with no time in it.
    pub fn build(workout: &Workout, athlete: &AthleteProfile) -> Option<Self> {
        let ftp = if athlete.ftp_watts > 0 {
            athlete.ftp_watts
        } else {
            AthleteProfile::default().ftp_watts
        };
        let mass = if athlete.weight_kg.is_finite() && athlete.weight_kg > 0.0 {
            athlete.weight_kg
        } else {
            AthleteProfile::default().weight_kg
        };

        let mut pieces = Vec::new();
        let (mut t, mut d) = (0.0f32, 0.0f32);
        let mut push = |secs: u32, pct: f32| {
            if secs == 0 {
                return;
            }
            let gradient = gradient_for(pct);
            let watts = (pct.max(0.0) / 100.0) * ftp as f32;
            let speed_ms = Route::speed_from_power(watts, gradient, mass).max(MIN_SPEED_MS);
            pieces.push(Piece {
                start_secs: t,
                secs: secs as f32,
                start_m: d,
                speed_ms,
                gradient,
            });
            t += secs as f32;
            d += speed_ms * secs as f32;
        };
        for seg in &workout.segments {
            if seg.is_ramp() {
                let mut done = 0;
                while done < seg.duration_secs {
                    let secs = RAMP_PIECE_SECS.min(seg.duration_secs - done);
                    let mid = (done as f32 + secs as f32 / 2.0) / seg.duration_secs as f32;
                    let pct = seg.power_low_pct + (seg.power_high_pct - seg.power_low_pct) * mid;
                    push(secs, pct);
                    done += secs;
                }
            } else {
                push(seg.duration_secs, seg.power_low_pct);
            }
        }
        if pieces.is_empty() || d < POINT_SPACING_M {
            return None;
        }
        let route = lay_route(&workout.name, &pieces, d);
        Some(Self { route, pieces })
    }

    /// How far along the road the rider is after `elapsed_secs` of the
    /// workout, in metres. Holds at the end once the workout is over.
    pub fn distance_at(&self, elapsed_secs: f32) -> f32 {
        let t = clock(elapsed_secs);
        let Some(p) = self.piece_at(t) else {
            return 0.0;
        };
        let into = (t - p.start_secs).clamp(0.0, p.secs);
        (p.start_m + p.speed_ms * into).min(self.route.total_distance_m)
    }

    /// How fast the rider is going along the road at `elapsed_secs`, in m/s;
    /// zero once the workout is over.
    pub fn speed_at(&self, elapsed_secs: f32) -> f32 {
        let end = self.pieces.last().map_or(0.0, |p| p.start_secs + p.secs);
        let t = clock(elapsed_secs);
        if t >= end {
            return 0.0;
        }
        self.piece_at(t).map_or(0.0, |p| p.speed_ms)
    }

    fn piece_at(&self, t: f32) -> Option<&Piece> {
        let i = self.pieces.partition_point(|p| p.start_secs <= t);
        self.pieces.get(i.saturating_sub(1))
    }
}

/// A clock reading fit to look up: before the start, or not a number, is the
/// start.
fn clock(elapsed_secs: f32) -> f32 {
    if elapsed_secs.is_finite() {
        elapsed_secs.max(0.0)
    } else {
        0.0
    }
}

/// The road's points: due north from the start, one every
/// [`POINT_SPACING_M`], at heights that follow the pieces' gradients with
/// each change eased. The 3D view lays its own bends into a straight route
/// (`track::lay`), so it does not need any here.
fn lay_route(name: &str, pieces: &[Piece], total_m: f32) -> Route {
    let n = (total_m / POINT_SPACING_M).ceil() as usize + 1;
    let at = |i: usize| (i as f32 * POINT_SPACING_M).min(total_m);

    // The heights the pieces climb to, unsmoothed: rising in straight lines.
    let mut raw = Vec::with_capacity(n);
    let (mut k, mut climbed) = (0usize, 0.0f32);
    for i in 0..n {
        let d = at(i);
        while k + 1 < pieces.len() && pieces[k + 1].start_m <= d {
            let p = pieces[k];
            climbed += p.gradient * (pieces[k + 1].start_m - p.start_m);
            k += 1;
        }
        raw.push(climbed + pieces[k].gradient * (d - pieces[k].start_m));
    }

    // Eased by two passes of a moving average: a rising road stays rising,
    // and its start and end heights are kept.
    let half = (EASE_M / POINT_SPACING_M).round() as usize;
    let mut eased = raw;
    for _ in 0..2 {
        eased = (0..n)
            .map(|i| {
                let lo = i.saturating_sub(half);
                let hi = (i + half).min(n - 1);
                let sum: f32 = (lo..=hi).map(|j| eased[j]).sum();
                sum / (hi - lo + 1) as f32
            })
            .collect();
    }

    let mut points: Vec<RoutePoint> = (0..n)
        .map(|i| {
            let d = at(i);
            RoutePoint {
                lat: START_LAT + (f64::from(d) / EARTH_RADIUS_M).to_degrees(),
                lng: START_LNG,
                elevation_m: START_ELEVATION_M + eased[i],
                distance_m: d,
                gradient: 0.0,
            }
        })
        .collect();
    let mut gain = 0.0;
    for i in 0..n.saturating_sub(1) {
        let rise = points[i + 1].elevation_m - points[i].elevation_m;
        let run = points[i + 1].distance_m - points[i].distance_m;
        if run > 0.0 {
            points[i].gradient = rise / run;
        }
        gain += rise.max(0.0);
    }
    Route {
        name: name.to_string(),
        points,
        total_distance_m: total_m,
        total_gain_m: gain,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::workout::{Segment, WorkoutCategory};

    fn rider() -> AthleteProfile {
        AthleteProfile {
            ftp_watts: 250,
            weight_kg: 80.0,
            ..AthleteProfile::default()
        }
    }

    fn workout(segments: Vec<Segment>) -> Workout {
        Workout::from_segments("Test", "", WorkoutCategory::Custom, segments)
    }

    fn intervals() -> Workout {
        workout(vec![
            Segment::steady(300, 50.0, "Easy"),
            Segment::steady(240, 105.0, "Hard"),
            Segment::steady(120, 50.0, "Easy"),
        ])
    }

    #[test]
    fn should_be_flat_up_to_sixty_percent_and_steepest_from_one_twenty() {
        assert_eq!(gradient_for(0.0), 0.0);
        assert_eq!(gradient_for(FLAT_TO_PCT), 0.0);
        assert_eq!(gradient_for(90.0), STEEPEST / 2.0);
        assert_eq!(gradient_for(STEEPEST_FROM_PCT), STEEPEST);
        assert_eq!(gradient_for(400.0), STEEPEST);
        assert_eq!(gradient_for(-50.0), 0.0);
        assert_eq!(gradient_for(f32::NAN), 0.0);
        assert_eq!(gradient_for(f32::INFINITY), 0.0);
    }

    #[test]
    fn should_make_each_stretch_as_long_as_its_interval_at_its_target() {
        let road = WorkoutRoad::build(&intervals(), &rider()).unwrap();
        let easy = Route::speed_from_power(125.0, 0.0, 80.0);
        let hard = Route::speed_from_power(262.5, gradient_for(105.0), 80.0);
        assert!((road.distance_at(300.0) - easy * 300.0).abs() < 0.01);
        assert!((road.distance_at(540.0) - (easy * 300.0 + hard * 240.0)).abs() < 0.01);
        let total = easy * 420.0 + hard * 240.0;
        assert!((road.route.total_distance_m - total).abs() < 0.01);
        assert_eq!(road.speed_at(299.0), easy);
        assert_eq!(road.speed_at(300.0), hard);
        assert_eq!(road.speed_at(539.9), hard);
        assert_eq!(road.speed_at(540.0), easy);
    }

    #[test]
    fn should_climb_through_the_hard_interval_and_level_out_either_side() {
        let road = WorkoutRoad::build(&intervals(), &rider()).unwrap();
        let r = &road.route;
        let (start, end) = (road.distance_at(300.0), road.distance_at(540.0));
        // Well inside the easy stretches, past the easing, the road is flat.
        assert_eq!(r.elevation_at(start - 200.0), START_ELEVATION_M);
        let top = r.elevation_at(end + 200.0);
        assert_eq!(r.elevation_at(r.total_distance_m), top);
        // The climb is the hard stretch's length at its gradient, eased or not.
        let rise = gradient_for(105.0) * (end - start);
        assert!((top - START_ELEVATION_M - rise).abs() < 0.01, "{top}");
        assert!((r.total_gain_m - rise).abs() < 0.01);
        // And mid-climb it is at its gradient.
        let mid = (start + end) / 2.0;
        assert!((r.gradient_at(mid) - gradient_for(105.0)).abs() < 0.001);
    }

    #[test]
    fn should_never_go_downhill() {
        let w = workout(vec![
            Segment::steady(60, 130.0, "a"),
            Segment::steady(20, 40.0, "b"),
            Segment::steady(30, 150.0, "c"),
            Segment::ramp(90, 120.0, 40.0, "d"),
        ]);
        let road = WorkoutRoad::build(&w, &rider()).unwrap();
        for p in road.route.points.windows(2) {
            assert!(p[1].elevation_m >= p[0].elevation_m - 1e-3);
        }
    }

    #[test]
    fn should_steepen_a_warm_up_ramp_piece_by_piece() {
        let w = workout(vec![Segment::ramp(600, 40.0, 100.0, "Warm-up")]);
        let road = WorkoutRoad::build(&w, &rider()).unwrap();
        // 40 pieces of 15 s; the first at 40.75 %, the last at 99.25 %.
        assert_eq!(road.pieces.len(), 40);
        assert_eq!(road.pieces[0].gradient, 0.0);
        assert_eq!(road.pieces[39].gradient, gradient_for(99.25));
        assert!(road.speed_at(10.0) > road.speed_at(590.0));
    }

    #[test]
    fn should_put_a_skip_straight_onto_the_next_stretch() {
        let road = WorkoutRoad::build(&intervals(), &rider()).unwrap();
        // The engine jumps the clock; the road jumps with it.
        assert_eq!(road.distance_at(300.0), road.pieces[1].start_m);
    }

    #[test]
    fn should_hold_at_the_end_once_the_workout_is_over() {
        let road = WorkoutRoad::build(&intervals(), &rider()).unwrap();
        let end = road.route.total_distance_m;
        assert_eq!(road.distance_at(660.0), end);
        assert_eq!(road.distance_at(5_000.0), end);
        assert_eq!(road.speed_at(660.0), 0.0);
        assert_eq!(road.distance_at(-30.0), 0.0);
        assert_eq!(road.distance_at(f32::NAN), 0.0);
        assert_eq!(road.speed_at(f32::NAN), road.pieces[0].speed_ms);
    }

    #[test]
    fn should_give_a_workout_with_no_time_in_it_no_road() {
        assert!(WorkoutRoad::build(&workout(vec![]), &rider()).is_none());
        let empty = workout(vec![Segment::steady(0, 100.0, "nothing")]);
        assert!(WorkoutRoad::build(&empty, &rider()).is_none());
    }

    #[test]
    fn should_still_lay_a_road_for_a_rider_with_no_ftp_or_weight() {
        let nobody = AthleteProfile {
            ftp_watts: 0,
            weight_kg: f32::NAN,
            ..AthleteProfile::default()
        };
        let road = WorkoutRoad::build(&intervals(), &nobody).unwrap();
        assert!(road.route.total_distance_m > 1_000.0);
        // A zero target still moves at the floor, so its time is on the road.
        let rest = workout(vec![Segment::steady(100, 0.0, "Stop")]);
        let road = WorkoutRoad::build(&rest, &rider()).unwrap();
        assert_eq!(road.route.total_distance_m, MIN_SPEED_MS * 100.0);
    }

    #[test]
    fn should_space_its_points_as_far_apart_as_the_ride_measures_them() {
        let road = WorkoutRoad::build(&intervals(), &rider()).unwrap();
        let pts = &road.route.points;
        for w in pts.windows(2) {
            let measured = crate::data::route::haversine(w[0].lat, w[0].lng, w[1].lat, w[1].lng);
            let said = w[1].distance_m - w[0].distance_m;
            assert!((measured - said).abs() < 0.01, "{measured} vs {said}");
        }
        assert_eq!(pts.last().unwrap().distance_m, road.route.total_distance_m);
    }
}
