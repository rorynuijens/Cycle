//! The ghost rider: the rider's fastest finished ride of a route, ridden
//! again beside them.
//!
//! A ride records one point for each second of riding — none while paused —
//! with the speed the ride moved at. Added up, those speeds are how far along
//! the route the ride was after each second of riding; scaled onto the
//! route's length, that is the ghost. It never feeds the ride (rule 5): it
//! is drawn, and its gap read out, and nothing else.
//!
//! Pure: no GTK, no database.

use crate::data::session::{DataPoint, Session};

/// A ride whose speeds add up to within this share of the route's length
/// finished it; one that falls further short was given up.
const FINISHED_WITHIN: f32 = 0.05;

/// A past ride, replayed against the clock of riding time.
#[derive(Debug, Clone, PartialEq)]
pub struct Ghost {
    /// Metres along the route after each whole second of riding, from 0 at
    /// the start to the route's length at the finish. Never falls.
    at: Vec<f32>,
}

/// How far the rider is from the ghost. Positive: behind it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gap {
    /// How much later the rider got here than the ghost did, in seconds.
    pub secs: f32,
    /// How far ahead of the rider the ghost is now, in metres.
    pub metres: f32,
}

impl Ghost {
    /// The ghost of a ride with `points` over a route `length_m` long, or
    /// `None` for a ride that did not finish the route.
    pub fn from_ride(points: &[DataPoint], length_m: f32) -> Option<Self> {
        if !(length_m.is_finite() && length_m > 0.0) {
            return None;
        }
        let mut order: Vec<&DataPoint> = points.iter().collect();
        order.sort_by_key(|p| p.elapsed_secs);
        let mut at = Vec::with_capacity(order.len() + 1);
        at.push(0.0f32);
        let mut d = 0.0f32;
        for p in order {
            let kmh = p
                .speed_kmh
                .filter(|v| v.is_finite() && *v > 0.0)
                .unwrap_or(0.0);
            d += kmh / 3.6;
            at.push(d);
        }
        // A hair of slack, so a ride exactly on the boundary is not lost to
        // a rounding in the sum.
        if d <= 0.0 || ((d / length_m) - 1.0).abs() > FINISHED_WITHIN + 1e-4 {
            return None;
        }
        let scale = length_m / d;
        for v in &mut at {
            *v *= scale;
        }
        Some(Self { at })
    }

    /// Seconds of riding the ghost took to finish.
    pub fn finish_secs(&self) -> u32 {
        (self.at.len() - 1) as u32
    }

    fn length_m(&self) -> f32 {
        self.at[self.at.len() - 1]
    }

    /// Where the ghost is after `secs` of riding: at the start before it, at
    /// the finish after it.
    pub fn distance_at(&self, secs: f32) -> f32 {
        if !(secs.is_finite() && secs > 0.0) {
            return 0.0;
        }
        let k = secs.floor() as usize;
        if k + 1 >= self.at.len() {
            return self.length_m();
        }
        let f = secs - k as f32;
        self.at[k] + (self.at[k + 1] - self.at[k]) * f
    }

    /// When the ghost first got to `metres` along the route, in seconds of
    /// riding.
    pub fn time_at(&self, metres: f32) -> f32 {
        if !(metres.is_finite() && metres > 0.0) {
            return 0.0;
        }
        if metres >= self.length_m() {
            return self.finish_secs() as f32;
        }
        // The first second it is at least that far.
        let k = self.at.partition_point(|&d| d < metres);
        let (a, b) = (self.at[k - 1], self.at[k]);
        (k - 1) as f32 + (metres - a) / (b - a)
    }

    /// The rider, `rider_m` along the route after `rider_secs` of riding,
    /// against the ghost.
    pub fn gap(&self, rider_m: f32, rider_secs: f32) -> Gap {
        Gap {
            secs: rider_secs - self.time_at(rider_m),
            metres: self.distance_at(rider_secs) - rider_m,
        }
    }
}

/// The gap as the cockpit shows it: `+0:12` behind, `−0:08` ahead, `0:00`
/// level — whole seconds, a true minus sign.
pub fn format_gap(secs: f32) -> String {
    let whole = if secs.is_finite() { secs.round() } else { 0.0 };
    if whole == 0.0 {
        return "0:00".into();
    }
    let sign = if whole > 0.0 { '+' } else { '−' };
    let n = whole.abs() as u32;
    format!("{sign}{}:{:02}", n / 60, n % 60)
}

/// The ride to race on the route `route_id`, `length_m` long: the fastest of
/// `rides` that rode it to the finish under the rider's own power — not an
/// FTP test, not moved by ERG emulation. An equal time does not take the
/// ghost from the ride that set it first.
pub fn fastest<'a>(
    rides: impl IntoIterator<Item = &'a Session>,
    route_id: i64,
    length_m: f32,
) -> Option<(&'a Session, Ghost)> {
    let mut best: Option<(&Session, Ghost)> = None;
    for s in rides {
        if s.route_id != Some(route_id) || !s.rider_driven || s.ended_at.is_none() || s.is_ftp_test
        {
            continue;
        }
        let Some(ghost) = Ghost::from_ride(&s.data_points, length_m) else {
            continue;
        };
        let better = match &best {
            None => true,
            Some((held, g)) => {
                ghost.finish_secs() < g.finish_secs()
                    || (ghost.finish_secs() == g.finish_secs() && s.started_at < held.started_at)
            }
        };
        if better {
            best = Some((s, ghost));
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(k: u32, kmh: f32) -> DataPoint {
        DataPoint {
            elapsed_secs: k,
            power_watts: Some(200),
            target_watts: None,
            heart_rate_bpm: None,
            cadence_rpm: None,
            speed_kmh: Some(kmh),
            lat: None,
            lng: None,
            altitude_m: None,
        }
    }

    /// `secs` seconds at a steady 36 km/h: 10 m a second.
    fn steady(secs: u32) -> Vec<DataPoint> {
        (1..=secs).map(|k| point(k, 36.0)).collect()
    }

    #[test]
    fn should_replay_a_steady_ride_second_by_second() {
        let g = Ghost::from_ride(&steady(100), 1_000.0).expect("a finished ride");
        assert_eq!(g.finish_secs(), 100);
        assert_eq!(g.distance_at(0.0), 0.0);
        assert_eq!(g.distance_at(10.0), 100.0);
        assert_eq!(g.distance_at(10.5), 105.0);
        assert_eq!(g.distance_at(100.0), 1_000.0);
        // Past its finish it waits there; before the start, at the start.
        assert_eq!(g.distance_at(500.0), 1_000.0);
        assert_eq!(g.distance_at(-3.0), 0.0);
        assert_eq!(g.time_at(250.0), 25.0);
        assert_eq!(g.time_at(1_000.0), 100.0);
        assert_eq!(g.time_at(5_000.0), 100.0);
        assert_eq!(g.time_at(0.0), 0.0);
    }

    #[test]
    fn should_land_the_ghost_on_the_routes_length_whatever_the_speeds_add_to() {
        // 1 020 m of speeds for a 1 000 m route: scaled, the finish is the
        // route's end, not 20 m past it.
        let g = Ghost::from_ride(&steady(102), 1_000.0).unwrap();
        assert!((g.distance_at(102.0) - 1_000.0).abs() < 1e-3);
        assert!((g.distance_at(51.0) - 500.0).abs() < 1e-3);
    }

    #[test]
    fn should_never_count_a_pause() {
        // A ride paused twice records no seconds while paused; however the
        // points are numbered, riding time is the count of points.
        let mut paused: Vec<DataPoint> = steady(100);
        for p in paused.iter_mut().skip(40) {
            p.elapsed_secs += 600;
        }
        assert_eq!(
            Ghost::from_ride(&paused, 1_000.0),
            Ghost::from_ride(&steady(100), 1_000.0)
        );
    }

    #[test]
    fn should_not_make_a_ghost_of_a_ride_given_up_short_of_the_finish() {
        assert!(Ghost::from_ride(&steady(94), 1_000.0).is_none());
        assert!(Ghost::from_ride(&steady(95), 1_000.0).is_some());
        assert!(Ghost::from_ride(&steady(106), 1_000.0).is_none());
        assert!(Ghost::from_ride(&[], 1_000.0).is_none());
        assert!(Ghost::from_ride(&steady(100), 0.0).is_none());
        assert!(Ghost::from_ride(&steady(100), f32::NAN).is_none());
    }

    #[test]
    fn should_wait_where_the_ride_stood_still() {
        // 20 s, then 10 s standing, then 80 s: time_at a spot it stood at is
        // when it first got there.
        let mut pts: Vec<DataPoint> = (1..=20).map(|k| point(k, 36.0)).collect();
        pts.extend((21..=30).map(|k| point(k, 0.0)));
        pts.extend((31..=110).map(|k| point(k, 36.0)));
        let g = Ghost::from_ride(&pts, 1_000.0).unwrap();
        assert_eq!(g.distance_at(25.0), 200.0);
        assert_eq!(g.time_at(200.0), 20.0);
        assert_eq!(g.time_at(210.0), 31.0);
    }

    #[test]
    fn should_read_the_gap_with_its_sign_and_exactly_zero_on_a_tie() {
        let g = Ghost::from_ride(&steady(100), 1_000.0).unwrap();
        let level = g.gap(500.0, 50.0);
        assert_eq!(
            level,
            Gap {
                secs: 0.0,
                metres: 0.0
            }
        );
        // Twelve seconds late to 500 m: behind, and the ghost 120 m on.
        let behind = g.gap(500.0, 62.0);
        assert_eq!(
            behind,
            Gap {
                secs: 12.0,
                metres: 120.0
            }
        );
        // At 500 m in 42 s: ahead by 8 s, the ghost 80 m back.
        let ahead = g.gap(500.0, 42.0);
        assert_eq!(
            ahead,
            Gap {
                secs: -8.0,
                metres: -80.0
            }
        );
    }

    #[test]
    fn should_write_the_gap_as_the_cockpit_shows_it() {
        assert_eq!(format_gap(12.0), "+0:12");
        assert_eq!(format_gap(-8.0), "−0:08");
        assert_eq!(format_gap(0.4), "0:00");
        assert_eq!(format_gap(-0.4), "0:00");
        assert_eq!(format_gap(0.5), "+0:01");
        assert_eq!(format_gap(-75.0), "−1:15");
        assert_eq!(format_gap(f32::NAN), "0:00");
    }

    fn ride_of(route_id: i64, secs: u32, minute: i64, driven: bool) -> Session {
        let mut s = Session::new(None);
        s.route_id = Some(route_id);
        s.rider_driven = driven;
        s.started_at = chrono::DateTime::UNIX_EPOCH + chrono::Duration::minutes(minute);
        s.ended_at = Some(s.started_at + chrono::Duration::seconds(i64::from(secs)));
        // `secs` seconds over 1 000 m.
        s.data_points = (1..=secs)
            .map(|k| point(k, 3_600.0 / secs as f32))
            .collect();
        s
    }

    #[test]
    fn should_race_the_fastest_finished_ride_the_rider_drove() {
        let mut erg = ride_of(7, 80, 1, false);
        erg.data_points
            .iter_mut()
            .for_each(|p| p.speed_kmh = Some(45.0));
        let mut test = ride_of(7, 85, 2, true);
        test.is_ftp_test = true;
        let mut unfinished = ride_of(7, 90, 3, true);
        unfinished.ended_at = None;
        let mut given_up = ride_of(7, 70, 4, true);
        given_up.data_points.truncate(50);
        let other_route = ride_of(8, 60, 5, true);
        let slow = ride_of(7, 120, 6, true);
        let best = ride_of(7, 100, 7, true);
        let rides = [erg, test, unfinished, given_up, other_route, slow, best];

        let (ride, ghost) = fastest(&rides, 7, 1_000.0).expect("a ghost");
        assert_eq!(ghost.finish_secs(), 100);
        assert_eq!(ride.started_at, rides[6].started_at);
        assert!(fastest(&rides, 9, 1_000.0).is_none());
    }

    #[test]
    fn should_leave_the_ghost_with_the_ride_that_set_the_time_first() {
        let first = ride_of(7, 100, 1, true);
        let equal = ride_of(7, 100, 9, true);
        let rides = [equal, first];
        let (ride, _) = fastest(&rides, 7, 1_000.0).unwrap();
        assert_eq!(ride.started_at, rides[1].started_at);
    }
}
