//! What the road ahead is about to do, in words.
//!
//! The route cockpit reports the gradient under the wheels right now, and the
//! profile draws the whole course, but neither answers the question a rider
//! actually asks on a route: *what is coming, and when?* A number that reads
//! "0.4 %" tells you nothing about the 6 % kilometre that starts in three
//! hundred metres, and on a forty-kilometre profile squeezed into a strip that
//! kilometre is a few pixels wide.
//!
//! This module reads the terrain ahead and names it. Everything here is derived
//! from the route file, so cues cost nothing per ride and appear whether or not
//! an API key is set — the same bargain [`super::cues`] makes for workouts.
//!
//! Deliberately free of time estimates. How long a climb takes depends on the
//! power the rider has not produced yet, and a prediction that drifts as they
//! ride is worse than no prediction.

use crate::data::route::Route;

/// Step between gradient samples when reading the road ahead, in metres.
///
/// Matches the profile chart's sampling and the ±25 m window
/// [`Route::gradient_at`] smooths over, so a cue and the chart beside it never
/// disagree about where a climb begins.
const SAMPLE_STEP_M: f32 = 25.0;

/// How far ahead the road is read, in metres.
///
/// Far enough to see a climb coming with time to change gear, short enough that
/// the cue is about the next thing rather than the whole ride.
const LOOKAHEAD_M: f32 = 3000.0;

/// Below this gradient, in percent, the road is flat as far as a rider is
/// concerned. The same ±1 % band [`crate::ui::widgets::zone_color::gradient_rgb`]
/// paints as flat, so the words and the colours agree.
const FLAT_BAND_PCT: f32 = 1.0;

/// A stretch shorter than this is a bump, not terrain worth announcing.
const MIN_SEGMENT_M: f32 = 150.0;

/// A climb this close counts as under way rather than approaching.
const IMMINENT_M: f32 = 100.0;

/// What the next stretch of road does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerrainAhead {
    Climb,
    Descent,
    Flat,
}

impl TerrainAhead {
    /// True for terrain the rider has to ride *at*, rather than through — the
    /// player emphasises those cues the way it emphasises a work interval.
    pub fn is_effort(&self) -> bool {
        matches!(self, TerrainAhead::Climb)
    }

    fn from_gradient_pct(pct: f32) -> Self {
        if pct > FLAT_BAND_PCT {
            TerrainAhead::Climb
        } else if pct < -FLAT_BAND_PCT {
            TerrainAhead::Descent
        } else {
            TerrainAhead::Flat
        }
    }
}

/// One stretch of like terrain read off the road ahead.
#[derive(Debug, Clone, PartialEq)]
pub struct CourseCue {
    pub terrain: TerrainAhead,
    /// Where the stretch begins, as distance from the route start in metres.
    pub starts_at_m: f32,
    /// How long the stretch runs, in metres.
    pub length_m: f32,
    /// Mean gradient over the stretch, in percent. Negative on a descent.
    pub mean_gradient_pct: f32,
    /// Shown large: what the road does and for how long.
    pub headline: String,
    /// A quieter second line: how far off it is, or how far into it the rider is.
    pub detail: String,
}

impl CourseCue {
    /// True once the rider is inside the stretch this cue describes.
    pub fn is_under_way(&self, distance_m: f32) -> bool {
        distance_m + IMMINENT_M >= self.starts_at_m
    }
}

/// The next stretch of terrain worth naming from `distance_m` onwards, or
/// `None` past the end of the route or when nothing ahead is distinctive.
///
/// A climb or a descent always wins over the flat it interrupts: a rider on a
/// flat road wants to know about the climb ahead, not to be told the flat
/// continues. Only when there is no climb or descent within [`LOOKAHEAD_M`]
/// does the flat itself get announced.
pub fn cue_at(route: &Route, distance_m: f32) -> Option<CourseCue> {
    let total = route.total_distance_m;
    if route.points.len() < 2 || total <= 0.0 || distance_m >= total {
        return None;
    }

    let segments = segments_ahead(route, distance_m);
    let pick = segments
        .iter()
        .find(|s| s.terrain != TerrainAhead::Flat)
        .or_else(|| segments.first())?;

    Some(describe(pick, distance_m))
}

/// A stretch of like terrain, before it is put into words.
struct Segment {
    terrain: TerrainAhead,
    start_m: f32,
    end_m: f32,
    /// Sum of sampled gradients, divided down when the mean is needed.
    gradient_sum_pct: f32,
    samples: u32,
}

impl Segment {
    fn mean_pct(&self) -> f32 {
        if self.samples == 0 {
            0.0
        } else {
            self.gradient_sum_pct / self.samples as f32
        }
    }

    fn length_m(&self) -> f32 {
        self.end_m - self.start_m
    }
}

/// Walk the road ahead, coalescing runs of like terrain into segments and
/// dropping the ones too short to be worth a word.
///
/// A short bump inside a longer stretch is absorbed rather than allowed to split
/// it: real roads undulate, and a climb that a rider experiences as one climb
/// should be announced as one climb.
fn segments_ahead(route: &Route, from_m: f32) -> Vec<Segment> {
    let end = (from_m + LOOKAHEAD_M).min(route.total_distance_m);
    let mut raw: Vec<Segment> = Vec::new();

    let mut d = from_m;
    while d <= end {
        let pct = route.gradient_at(d) * 100.0;
        let terrain = TerrainAhead::from_gradient_pct(pct);
        match raw.last_mut() {
            Some(seg) if seg.terrain == terrain => {
                seg.end_m = d;
                seg.gradient_sum_pct += pct;
                seg.samples += 1;
            }
            _ => raw.push(Segment {
                terrain,
                start_m: d,
                end_m: d,
                gradient_sum_pct: pct,
                samples: 1,
            }),
        }
        d += SAMPLE_STEP_M;
    }

    // Absorb stretches too short to name into whatever precedes them, so one
    // undulating climb reads as a climb rather than as four fragments. Absorbing
    // a shelf leaves the climb either side of it adjacent, so like terrain
    // rejoins too — otherwise the shelf would still split the climb in two, just
    // silently.
    let mut merged: Vec<Segment> = Vec::new();
    for seg in raw {
        match merged.last_mut() {
            Some(prev) if seg.length_m() < MIN_SEGMENT_M || prev.terrain == seg.terrain => {
                prev.end_m = seg.end_m;
                prev.gradient_sum_pct += seg.gradient_sum_pct;
                prev.samples += seg.samples;
            }
            _ => merged.push(seg),
        }
    }
    // The absorbing may have turned a leading fragment into the whole answer;
    // drop it only if it is still too short and something follows it.
    if merged.len() > 1 && merged[0].length_m() < MIN_SEGMENT_M {
        merged.remove(0);
    }
    merged
}

/// Put a segment into the two lines the cockpit shows.
fn describe(seg: &Segment, distance_m: f32) -> CourseCue {
    let mean = seg.mean_pct();
    let length = seg.length_m();
    let away = (seg.start_m - distance_m).max(0.0);
    let under_way = away <= IMMINENT_M;

    let headline = match seg.terrain {
        TerrainAhead::Climb => format!("Climb — {} at {:.1}%", format_distance(length), mean.abs()),
        TerrainAhead::Descent => format!("Descent — {}", format_distance(length)),
        TerrainAhead::Flat => format!("Flat — {}", format_distance(length)),
    };

    let detail = if under_way {
        let done = (distance_m - seg.start_m).max(0.0);
        let left = (length - done).max(0.0);
        match seg.terrain {
            TerrainAhead::Climb => format!("{} to the top", format_distance(left)),
            _ => format!("{} to go", format_distance(left)),
        }
    } else {
        match seg.terrain {
            TerrainAhead::Climb => format!("starts in {}", format_distance(away)),
            TerrainAhead::Descent => format!("begins in {}", format_distance(away)),
            TerrainAhead::Flat => format!("in {}", format_distance(away)),
        }
    };

    CourseCue {
        terrain: seg.terrain,
        starts_at_m: seg.start_m,
        length_m: length,
        mean_gradient_pct: mean,
        headline,
        detail,
    }
}

/// Metres below a kilometre, kilometres above it — the way a rider reads a road
/// sign, and the way the rest of the cockpit already writes distances.
fn format_distance(metres: f32) -> String {
    if metres < 1000.0 {
        format!("{} m", ((metres / 50.0).round() * 50.0) as i32)
    } else {
        format!("{:.1} km", metres / 1000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::route::RoutePoint;

    /// Build a route from `(distance_m, elevation_m)` pairs. Coordinates are
    /// irrelevant to every cue decision, so they only have to be plausible.
    fn route_from(profile: &[(f32, f32)]) -> Route {
        let points: Vec<RoutePoint> = profile
            .iter()
            .map(|&(distance_m, elevation_m)| RoutePoint {
                lat: 51.0,
                lng: -0.1,
                elevation_m,
                distance_m,
                gradient: 0.0,
            })
            .collect();
        let total = profile.last().map(|p| p.0).unwrap_or(0.0);
        Route {
            name: "Test".into(),
            points,
            total_distance_m: total,
            total_gain_m: 0.0,
        }
    }

    /// 2 km of flat, then a 1 km climb at 6 %, then 2 km of flat.
    fn ramp_route() -> Route {
        route_from(&[
            (0.0, 100.0),
            (2000.0, 100.0),
            (3000.0, 160.0), // 60 m over 1000 m = 6%
            (5000.0, 160.0),
        ])
    }

    #[test]
    fn should_report_no_climb_when_route_is_flat() {
        let route = route_from(&[(0.0, 100.0), (5000.0, 100.0)]);
        let cue = cue_at(&route, 0.0).expect("a flat route still describes itself");
        assert_eq!(cue.terrain, TerrainAhead::Flat);
        assert!(!cue.terrain.is_effort());
    }

    #[test]
    fn should_name_the_climb_when_one_is_ahead() {
        let route = ramp_route();
        let cue = cue_at(&route, 500.0).expect("the ramp is within lookahead");
        assert_eq!(cue.terrain, TerrainAhead::Climb);
        assert!(
            (cue.mean_gradient_pct - 6.0).abs() < 1.0,
            "expected ~6%, got {:.2}%",
            cue.mean_gradient_pct
        );
        assert!(
            (cue.length_m - 1000.0).abs() < 200.0,
            "expected ~1000 m, got {:.0} m",
            cue.length_m
        );
    }

    #[test]
    fn should_prefer_a_climb_over_the_flat_it_interrupts() {
        let route = ramp_route();
        // Standing at the start, the flat underfoot runs for 2 km — but the
        // climb is what the rider needs to know about.
        let cue = cue_at(&route, 0.0).expect("cue at the start");
        assert_eq!(cue.terrain, TerrainAhead::Climb);
        assert!(
            cue.detail.starts_with("starts in"),
            "expected an approach detail, got {:?}",
            cue.detail
        );
    }

    #[test]
    fn should_count_down_to_the_top_when_climb_is_under_way() {
        let route = ramp_route();
        let cue = cue_at(&route, 2500.0).expect("cue mid-climb");
        assert_eq!(cue.terrain, TerrainAhead::Climb);
        assert!(cue.is_under_way(2500.0));
        assert!(
            cue.detail.ends_with("to the top"),
            "expected a countdown to the top, got {:?}",
            cue.detail
        );
    }

    #[test]
    fn should_report_a_descent_as_a_descent() {
        let route = route_from(&[(0.0, 200.0), (500.0, 200.0), (2000.0, 110.0)]);
        let cue = cue_at(&route, 0.0).expect("the drop is within lookahead");
        assert_eq!(cue.terrain, TerrainAhead::Descent);
        assert!(cue.mean_gradient_pct < 0.0);
    }

    #[test]
    fn should_return_none_when_past_the_end_of_the_route() {
        let route = ramp_route();
        assert!(cue_at(&route, 5000.0).is_none());
        assert!(cue_at(&route, 9999.0).is_none());
    }

    #[test]
    fn should_return_none_when_route_has_too_few_points() {
        let route = route_from(&[(0.0, 100.0)]);
        assert!(cue_at(&route, 0.0).is_none());
    }

    // ── The flat band, which the profile colours must agree with ─────────────

    #[test]
    fn should_read_as_flat_at_exactly_one_percent() {
        assert_eq!(TerrainAhead::from_gradient_pct(1.0), TerrainAhead::Flat);
        assert_eq!(TerrainAhead::from_gradient_pct(-1.0), TerrainAhead::Flat);
    }

    #[test]
    fn should_read_as_a_climb_just_above_one_percent() {
        assert_eq!(TerrainAhead::from_gradient_pct(1.01), TerrainAhead::Climb);
    }

    #[test]
    fn should_read_as_a_descent_just_below_minus_one_percent() {
        assert_eq!(
            TerrainAhead::from_gradient_pct(-1.01),
            TerrainAhead::Descent
        );
    }

    // ── Distance wording ─────────────────────────────────────────────────────

    #[test]
    fn should_write_metres_below_a_kilometre_and_km_above() {
        assert_eq!(format_distance(0.0), "0 m");
        assert_eq!(format_distance(320.0), "300 m");
        assert_eq!(format_distance(340.0), "350 m");
        assert_eq!(format_distance(1000.0), "1.0 km");
        assert_eq!(format_distance(2450.0), "2.5 km");
    }

    #[test]
    fn should_absorb_a_short_bump_into_the_climb_around_it() {
        // A climb with a 50 m flat shelf partway up reads as one climb.
        let route = route_from(&[
            (0.0, 100.0),
            (400.0, 124.0),
            (450.0, 124.0), // shelf
            (1000.0, 157.0),
        ]);
        let cue = cue_at(&route, 0.0).expect("cue at the start");
        assert_eq!(cue.terrain, TerrainAhead::Climb);
        assert!(
            cue.length_m > 700.0,
            "the shelf should not split the climb, got {:.0} m",
            cue.length_m
        );
        // The shelf drags the mean down from 6 % but nowhere near to zero, and
        // certainly not below it: absorbing a segment adds its gradients to the
        // one before, and its sample count with them.
        assert!(
            (4.0..=6.5).contains(&cue.mean_gradient_pct),
            "mean gradient was {}",
            cue.mean_gradient_pct
        );
    }

    // ── The two lines the cockpit shows ─────────────────────────────────────
    //
    // Mutation testing found the distance arithmetic in `describe` and the
    // "am I on it yet" test both unexercised: the tests here checked which
    // terrain was picked, never what the rider was told about it.

    fn a_cue(starts_at_m: f32) -> CourseCue {
        CourseCue {
            terrain: TerrainAhead::Climb,
            starts_at_m,
            length_m: 500.0,
            mean_gradient_pct: 6.0,
            headline: String::new(),
            detail: String::new(),
        }
    }

    #[test]
    fn should_treat_only_a_climb_as_an_effort() {
        assert!(TerrainAhead::Climb.is_effort());
        assert!(!TerrainAhead::Descent.is_effort());
        assert!(!TerrainAhead::Flat.is_effort());
    }

    #[test]
    fn should_call_a_stretch_under_way_once_it_is_imminent() {
        let cue = a_cue(200.0);
        // A hundred metres short of the start already counts: the rider needs
        // the cue before the gradient arrives, not as it does.
        assert!(!cue.is_under_way(99.0));
        assert!(cue.is_under_way(100.0));
        assert!(cue.is_under_way(400.0));
    }

    #[test]
    fn should_say_how_far_off_a_climb_is_before_it_starts() {
        let route = ramp_route();
        let at = 500.0;
        let cue = cue_at(&route, at).expect("the climb ahead");
        assert_eq!(cue.terrain, TerrainAhead::Climb);
        let away = format_distance(cue.starts_at_m - at);
        assert!(
            cue.detail.contains(&away),
            "expected the climb {away} off, detail was {:?}",
            cue.detail
        );
    }

    #[test]
    fn should_say_how_far_is_left_to_the_top() {
        let route = ramp_route();
        let at = 2400.0;
        let cue = cue_at(&route, at).expect("the climb");
        assert_eq!(cue.terrain, TerrainAhead::Climb);
        assert!(cue.is_under_way(at));
        let left = format_distance(cue.length_m - (at - cue.starts_at_m));
        assert!(
            cue.detail.contains(&left),
            "expected {left} to the top, detail was {:?}",
            cue.detail
        );
        assert!(
            cue.detail.contains("to the top"),
            "detail was {:?}",
            cue.detail
        );
    }

    #[test]
    fn should_have_nothing_to_say_past_the_end_of_the_route() {
        let route = ramp_route();
        assert_eq!(cue_at(&route, route.total_distance_m), None);
        assert!(cue_at(&route, route.total_distance_m - 1.0).is_some());
    }

    #[test]
    fn should_have_nothing_to_say_about_a_route_with_no_length() {
        let route = route_from(&[(0.0, 100.0), (0.0, 100.0)]);
        assert_eq!(cue_at(&route, 0.0), None);
    }

    #[test]
    fn should_have_nothing_to_say_about_a_route_of_one_point() {
        let mut route = route_from(&[(0.0, 100.0)]);
        // A length it does not have the points to describe: the guard is on the
        // points, not only on the total.
        route.total_distance_m = 5000.0;
        assert_eq!(cue_at(&route, 0.0), None);
    }
}
