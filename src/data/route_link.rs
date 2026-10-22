//! Which saved route an old ride was on, and whether the rider's own effort
//! set its speed — for rides recorded before the ride carried either
//! (schema v9). Only a ride that is both is a time worth racing as a ghost.
//!
//! A route ride records the route position every second, so its track lies
//! along the route's line. Pure: no GTK, no database.

use crate::data::route::Route;
use crate::data::session::DataPoint;

/// A track point this close to the route's line is on it, in metres: well
/// over a GPS track's wander, well under the next road over.
const ON_ROUTE_M: f64 = 30.0;
/// This share of the track must lie on the route.
const ON_SHARE: f64 = 0.9;
/// And it must reach this close to either end of the route, as a share of
/// its length: a ride abandoned halfway is not a time for the whole route.
const END_SHARE: f32 = 0.02;
/// ERG emulation moves a rider at this pace, in km/h, whatever they do.
const NOMINAL_KMH: f32 = 25.0;
/// Below this, in km/h, the rider is standing still.
const MOVING_KMH: f32 = 0.5;

/// Metres east and north of `origin`: flat enough over a route's length.
fn local(origin: (f64, f64), lat: f64, lng: f64) -> (f64, f64) {
    const M_PER_DEG: f64 = 111_320.0;
    let east = (lng - origin.1) * M_PER_DEG * origin.0.to_radians().cos();
    let north = (lat - origin.0) * M_PER_DEG;
    (east, north)
}

/// Whether `track` — a ride's recorded positions — rode `route` from start to
/// finish.
pub fn rode(track: &[(f64, f64)], route: &Route) -> bool {
    let (Some(first), true) = (route.points.first(), route.points.len() >= 2) else {
        return false;
    };
    let total = route.total_distance_m;
    if track.is_empty() || !(total.is_finite() && total > 0.0) {
        return false;
    }
    let origin = (first.lat, first.lng);
    let line: Vec<((f64, f64), f32)> = route
        .points
        .iter()
        .map(|p| (local(origin, p.lat, p.lng), p.distance_m))
        .collect();
    let mut on = 0usize;
    let (mut lo, mut hi) = (f32::MAX, f32::MIN);
    for &(lat, lng) in track {
        let p = local(origin, lat, lng);
        // The nearest point on the line, and how far along the route it is.
        let mut best = (f64::MAX, 0.0f32);
        for pair in line.windows(2) {
            let ((a, da), (b, db)) = (pair[0], pair[1]);
            let (vx, vy) = (b.0 - a.0, b.1 - a.1);
            let len2 = vx * vx + vy * vy;
            let t = if len2 > 1e-9 {
                (((p.0 - a.0) * vx + (p.1 - a.1) * vy) / len2).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let (qx, qy) = (a.0 + vx * t, a.1 + vy * t);
            let d2 = (p.0 - qx).powi(2) + (p.1 - qy).powi(2);
            if d2 < best.0 {
                best = (d2, da + (db - da) * t as f32);
            }
        }
        if best.0 <= ON_ROUTE_M * ON_ROUTE_M {
            on += 1;
            lo = lo.min(best.1);
            hi = hi.max(best.1);
        }
    }
    on as f64 >= ON_SHARE * track.len() as f64
        && lo <= END_SHARE * total
        && hi >= (1.0 - END_SHARE) * total
}

/// Whether the rider's own effort set the ride's speed: it moved, and not
/// every moving second was at ERG emulation's nominal pace.
pub fn rider_driven(points: &[DataPoint]) -> bool {
    let moving: Vec<f32> = points
        .iter()
        .filter_map(|p| p.speed_kmh)
        .filter(|v| v.is_finite() && *v >= MOVING_KMH)
        .collect();
    !moving.is_empty() && moving.iter().any(|v| (v - NOMINAL_KMH).abs() > 0.01)
}

/// The track of a ride's points: those with a position.
pub fn track(points: &[DataPoint]) -> Vec<(f64, f64)> {
    points
        .iter()
        .filter_map(|p| Some((p.lat?, p.lng?)))
        .filter(|(lat, lng)| lat.is_finite() && lng.is_finite())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::route::RoutePoint;

    /// 2 km due north from 52° N, a point every 100 m.
    fn north() -> Route {
        let deg = 100.0 / 111_320.0;
        let points: Vec<RoutePoint> = (0..=20)
            .map(|k| RoutePoint {
                lat: 52.0 + k as f64 * deg,
                lng: 6.0,
                elevation_m: 10.0,
                distance_m: k as f32 * 100.0,
                gradient: 0.0,
            })
            .collect();
        Route {
            name: "North".into(),
            points,
            total_distance_m: 2_000.0,
            total_gain_m: 0.0,
        }
    }

    /// A ride along `north()` from `from` to `to` metres, a point every 5 m,
    /// `east` metres to the side.
    fn ride(from: f32, to: f32, east: f64) -> Vec<(f64, f64)> {
        let deg = 1.0 / 111_320.0;
        let lng_deg = deg / 52f64.to_radians().cos();
        let mut out = Vec::new();
        let mut d = from;
        while d <= to {
            out.push((52.0 + f64::from(d) * deg, 6.0 + east * lng_deg));
            d += 5.0;
        }
        out
    }

    #[test]
    fn should_know_a_ride_along_the_whole_route() {
        assert!(rode(&ride(0.0, 2_000.0, 0.0), &north()));
        // A GPS track off to one side, still on the road.
        assert!(rode(&ride(0.0, 2_000.0, 25.0), &north()));
    }

    #[test]
    fn should_not_take_the_next_road_over_for_the_route() {
        assert!(!rode(&ride(0.0, 2_000.0, 40.0), &north()));
    }

    #[test]
    fn should_not_count_a_ride_given_up_short_of_the_finish() {
        // On the route all the way — but only 1.9 km of its 2.
        assert!(!rode(&ride(0.0, 1_900.0, 0.0), &north()));
        // Exactly at the 2 % allowance either end, it counts.
        assert!(rode(&ride(40.0, 1_960.0, 0.0), &north()));
        assert!(!rode(&ride(45.0, 2_000.0, 0.0), &north()));
    }

    #[test]
    fn should_not_count_a_ride_that_wandered_off_for_long() {
        // Over a tenth of the track off the route: another ride that
        // happened to start and end here.
        let mut track = ride(0.0, 2_000.0, 0.0);
        let away = track.len() / 9;
        for p in track.iter_mut().skip(100).take(away) {
            p.1 += 0.01;
        }
        assert!(!rode(&track, &north()));
    }

    #[test]
    fn should_know_nothing_from_nothing() {
        assert!(!rode(&[], &north()));
        let mut empty = north();
        empty.points.truncate(1);
        assert!(!rode(&ride(0.0, 2_000.0, 0.0), &empty));
    }

    fn at(kmh: Option<f32>) -> DataPoint {
        DataPoint {
            elapsed_secs: 0,
            power_watts: Some(180),
            target_watts: None,
            heart_rate_bpm: None,
            cadence_rpm: None,
            speed_kmh: kmh,
            lat: None,
            lng: None,
            altitude_m: None,
        }
    }

    #[test]
    fn should_tell_the_riders_own_speed_from_the_nominal_pace() {
        // SIM physics: the speed wanders with the road and the legs.
        assert!(rider_driven(&[
            at(Some(28.4)),
            at(Some(31.0)),
            at(Some(0.0))
        ]));
        // ERG emulation: 25 km/h whenever moving, whatever happened.
        assert!(!rider_driven(&[
            at(Some(25.0)),
            at(Some(0.0)),
            at(Some(25.0))
        ]));
        // Never moved, or never said: nothing to race.
        assert!(!rider_driven(&[at(Some(0.0)), at(Some(0.3))]));
        assert!(!rider_driven(&[at(None)]));
        assert!(!rider_driven(&[]));
    }

    #[test]
    fn should_take_the_track_from_the_points_with_a_position() {
        let mut a = at(Some(20.0));
        a.lat = Some(52.0);
        a.lng = Some(6.0);
        let mut nan = a.clone();
        nan.lat = Some(f64::NAN);
        assert_eq!(track(&[a, at(None), nan]), vec![(52.0, 6.0)]);
    }
}
