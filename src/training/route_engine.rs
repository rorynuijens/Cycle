#![allow(dead_code)]

use crate::data::route::Route;

/// Maximum change in the grade sent to the trainer, in percentage points per second.
///
/// Real roads ramp into their gradient; stepping the trainer straight to a new
/// grade makes it lurch. At 1 %/s a 0 → 8 % wall arrives over 8 seconds.
const MAX_GRADE_SLEW_PCT_PER_S: f32 = 1.0;

/// Driving state for a route simulation.
pub struct RouteEngine {
    pub route: Route,
    /// Current position in metres from the route start.
    pub distance_m: f32,
    /// Speed set by the rider, in m/s.
    pub speed_ms: f32,
    /// Combined rider + bike mass in kg.
    pub mass_kg: f32,
    /// Elapsed simulation seconds.
    pub elapsed_secs: u32,
    /// Exponentially smoothed rider power (W) used by SIM mode — keeps virtual
    /// speed from jittering with every power-meter fluctuation.
    smoothed_power: f32,
    /// Rate-limited grade (%) last computed for the trainer — see
    /// [`RouteEngine::trainer_grade_percent`].
    trainer_grade_pct: f32,
}

impl RouteEngine {
    pub fn new(route: Route, speed_ms: f32, mass_kg: f32) -> Self {
        Self {
            route,
            distance_m: 0.0,
            speed_ms,
            mass_kg,
            elapsed_secs: 0,
            smoothed_power: 0.0,
            trainer_grade_pct: 0.0,
        }
    }

    /// Is the route finished?
    pub fn is_done(&self) -> bool {
        self.distance_m >= self.route.total_distance_m
    }

    /// Current gradient (fraction) at `distance_m`.
    pub fn current_gradient(&self) -> f32 {
        self.gradient_at(self.distance_m)
    }

    /// Grade (%) to send to the trainer: the current gradient, rate-limited to
    /// [`MAX_GRADE_SLEW_PCT_PER_S`] so resistance ramps rather than steps.
    ///
    /// Updated once per tick — the displayed gradient stays the true one from
    /// [`RouteEngine::current_gradient`].
    pub fn trainer_grade_percent(&self) -> f32 {
        self.trainer_grade_pct
    }

    /// Move the trainer grade one second closer to the gradient under the rider.
    fn slew_trainer_grade(&mut self) {
        let target = self.current_gradient() * 100.0;
        let delta = (target - self.trainer_grade_pct)
            .clamp(-MAX_GRADE_SLEW_PCT_PER_S, MAX_GRADE_SLEW_PCT_PER_S);
        self.trainer_grade_pct += delta;
    }

    /// Target power (W) at the current position and speed.
    pub fn current_target_watts(&self) -> u32 {
        Route::power_at(self.speed_ms, self.current_gradient(), self.mass_kg) as u32
    }

    /// Advance one simulated second. Returns target watts for that second.
    pub fn tick(&mut self) -> u32 {
        let watts = self.current_target_watts();
        self.distance_m = (self.distance_m + self.speed_ms).min(self.route.total_distance_m);
        self.elapsed_secs += 1;
        self.slew_trainer_grade();
        watts
    }

    /// Advance one simulated second in SIM mode: the rider's measured power is
    /// smoothed (~3 s EMA) and converted to a virtual speed for the current
    /// gradient, which drives position. Returns that speed in m/s.
    pub fn tick_sim(&mut self, power_watts: u32) -> f32 {
        const ALPHA: f32 = 0.3; // EMA weight ≈ 3-second smoothing at 1 Hz
        self.smoothed_power = self.smoothed_power * (1.0 - ALPHA) + power_watts as f32 * ALPHA;
        let speed =
            Route::speed_from_power(self.smoothed_power, self.current_gradient(), self.mass_kg);
        self.speed_ms = speed;
        self.distance_m = (self.distance_m + speed).min(self.route.total_distance_m);
        self.elapsed_secs += 1;
        self.slew_trainer_grade();
        speed
    }

    /// Speed (m/s) for ERG-emulation mode, where the trainer cannot follow the
    /// road and the ride is driven at a nominal pace instead.
    ///
    /// A rider producing nothing is not riding: with no power and no speed sensor
    /// to say otherwise, the route stops advancing. A real speed reading always
    /// wins, since a freewheeling flywheel is genuine movement.
    pub fn emulated_speed_ms(power_watts: Option<u32>, speed_kmh: Option<f32>) -> f32 {
        /// Nominal pace for ERG emulation — 25 km/h.
        const NOMINAL_SPEED_MS: f32 = 6.944;

        match speed_kmh {
            Some(kmh) => (kmh / 3.6).max(0.0),
            None if power_watts.unwrap_or(0) > 0 => NOMINAL_SPEED_MS,
            None => 0.0,
        }
    }

    /// Set the rider's speed (e.g. from a real-time speed sensor or ERG override).
    pub fn set_speed(&mut self, speed_ms: f32) {
        self.speed_ms = speed_ms.max(0.0);
    }

    /// Progress fraction [0.0, 1.0].
    pub fn progress(&self) -> f32 {
        if self.route.total_distance_m > 0.0 {
            (self.distance_m / self.route.total_distance_m).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    /// Remaining distance in metres.
    pub fn remaining_m(&self) -> f32 {
        (self.route.total_distance_m - self.distance_m).max(0.0)
    }

    /// The rider's current position on the route, if it has coordinates.
    pub fn current_position(&self) -> Option<(f64, f64)> {
        self.position_at(self.distance_m)
    }

    /// Current elevation in metres above sea level.
    pub fn current_elevation(&self) -> f32 {
        self.elevation_at(self.distance_m)
    }

    /// Position at `distance_m` by linear interpolation between route points.
    ///
    /// Interpolating in degrees is accurate at the metre scale between GPX
    /// points, which are only seconds of arc apart.
    pub fn position_at(&self, distance_m: f32) -> Option<(f64, f64)> {
        let pts = &self.route.points;
        let first = pts.first()?;
        let idx = pts.partition_point(|p| p.distance_m <= distance_m);
        if idx == 0 {
            return Some((first.lat, first.lng));
        }
        let Some(p1) = pts.get(idx) else {
            let last = pts.last()?;
            return Some((last.lat, last.lng));
        };
        let p0 = &pts[idx - 1];
        let span = p1.distance_m - p0.distance_m;
        if span < 0.1 {
            return Some((p0.lat, p0.lng));
        }
        let t = ((distance_m - p0.distance_m) / span) as f64;
        Some((
            p0.lat + t * (p1.lat - p0.lat),
            p0.lng + t * (p1.lng - p0.lng),
        ))
    }

    /// Elevation at `distance_m` — see [`Route::elevation_at`].
    pub fn elevation_at(&self, distance_m: f32) -> f32 {
        self.route.elevation_at(distance_m)
    }

    /// Gradient (fraction) at `distance_m` — see [`Route::gradient_at`].
    fn gradient_at(&self, distance_m: f32) -> f32 {
        self.route.gradient_at(distance_m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::route::RoutePoint;

    fn make_flat_route() -> Route {
        let points = vec![
            RoutePoint {
                lat: 51.0,
                lng: -0.1,
                elevation_m: 100.0,
                distance_m: 0.0,
                gradient: 0.0,
            },
            RoutePoint {
                lat: 51.0,
                lng: -0.11,
                elevation_m: 100.0,
                distance_m: 500.0,
                gradient: 0.0,
            },
            RoutePoint {
                lat: 51.0,
                lng: -0.12,
                elevation_m: 100.0,
                distance_m: 1000.0,
                gradient: 0.0,
            },
        ];
        Route {
            name: "Flat Test".into(),
            points,
            total_distance_m: 1000.0,
            total_gain_m: 0.0,
        }
    }

    #[test]
    fn tick_advances_distance() {
        let route = make_flat_route();
        let mut engine = RouteEngine::new(route, 5.0, 80.0);
        engine.tick();
        assert!((engine.distance_m - 5.0).abs() < 0.01);
    }

    #[test]
    fn done_after_enough_ticks() {
        let route = make_flat_route();
        let mut engine = RouteEngine::new(route, 10.0, 80.0);
        for _ in 0..200 {
            engine.tick();
        }
        assert!(engine.is_done());
    }

    #[test]
    fn progress_reaches_one() {
        let route = make_flat_route();
        let mut engine = RouteEngine::new(route, 10.0, 80.0);
        while !engine.is_done() {
            engine.tick();
        }
        assert!((engine.progress() - 1.0).abs() < 0.01);
    }

    // ── SIM mode ─────────────────────────────────────────────────────────────

    #[test]
    fn tick_sim_advances_with_power() {
        let route = make_flat_route();
        let mut engine = RouteEngine::new(route, 0.0, 75.0);
        for _ in 0..10 {
            engine.tick_sim(200);
        }
        // 10 s at ~200 W on the flat: EMA ramps up, but well past 50 m
        assert!(engine.distance_m > 50.0, "got {} m", engine.distance_m);
        assert!(engine.speed_ms > 8.0, "got {} m/s", engine.speed_ms);
    }

    #[test]
    fn tick_sim_stalls_without_power_on_the_flat() {
        let route = make_flat_route();
        let mut engine = RouteEngine::new(route, 0.0, 75.0);
        for _ in 0..10 {
            engine.tick_sim(0);
        }
        assert!(engine.distance_m < 2.0, "got {} m", engine.distance_m);
    }

    #[test]
    fn tick_sim_smooths_power_spikes() {
        let route = make_flat_route();
        let mut engine = RouteEngine::new(route, 0.0, 75.0);
        for _ in 0..30 {
            engine.tick_sim(200);
        }
        let settled = engine.speed_ms;
        engine.tick_sim(800); // one-second spike
        let after_spike = engine.speed_ms;
        // Speed rises, but nowhere near the steady-state speed of 800 W (~15 m/s)
        assert!(after_spike > settled);
        assert!(
            after_spike < settled + 3.0,
            "spike passed through unsmoothed"
        );
    }

    // ── ERG-emulation speed ──────────────────────────────────────────────────

    #[test]
    fn emulated_speed_is_zero_without_power() {
        assert_eq!(RouteEngine::emulated_speed_ms(None, None), 0.0);
        assert_eq!(RouteEngine::emulated_speed_ms(Some(0), None), 0.0);
    }

    #[test]
    fn emulated_speed_runs_at_the_nominal_pace_while_pedalling() {
        let speed = RouteEngine::emulated_speed_ms(Some(150), None);
        assert!((speed - 6.944).abs() < 0.001, "got {speed}");
    }

    #[test]
    fn a_real_speed_reading_wins_over_the_nominal_pace() {
        // A freewheeling flywheel is genuine movement, power or no power.
        let speed = RouteEngine::emulated_speed_ms(None, Some(18.0));
        assert!((speed - 5.0).abs() < 0.001, "got {speed}");
        let stopped = RouteEngine::emulated_speed_ms(Some(200), Some(0.0));
        assert_eq!(stopped, 0.0, "a sensor reporting a standstill is believed");
    }

    // ── Gradient smoothing ───────────────────────────────────────────────────

    /// A route with a sharp elevation step between two closely spaced points —
    /// the shape a noisy GPX produces and the old per-point gradient turned into
    /// an instant resistance change.
    fn make_step_route() -> Route {
        let mut points = Vec::new();
        for i in 0..=20 {
            let distance_m = i as f32 * 10.0;
            // Flat for 100 m, then a 10 m rise of 5 m (50% point gradient), then flat.
            let elevation_m = match distance_m {
                d if d <= 100.0 => 100.0,
                d if d <= 110.0 => 100.0 + (d - 100.0) / 2.0,
                _ => 105.0,
            };
            points.push(RoutePoint {
                lat: 51.0,
                lng: -0.1,
                elevation_m,
                distance_m,
                gradient: 0.0,
            });
        }
        Route {
            name: "Step Test".into(),
            points,
            total_distance_m: 200.0,
            total_gain_m: 5.0,
        }
    }

    #[test]
    fn gradient_spreads_the_step_over_the_window() {
        let engine = RouteEngine::new(make_step_route(), 0.0, 75.0);
        // 5 m of rise measured over a 50 m window is 10%, not the 50% the raw
        // point-to-point gradient would report.
        let peak = engine.gradient_at(105.0);
        assert!((peak - 0.10).abs() < 0.01, "got {peak}");
        // The climb is felt before it arrives and after it ends — that is the smoothing.
        assert!(engine.gradient_at(85.0) > 0.0);
        assert!(engine.gradient_at(125.0) > 0.0);
    }

    #[test]
    fn gradient_is_continuous_along_the_route() {
        let engine = RouteEngine::new(make_step_route(), 0.0, 75.0);
        let mut prev = engine.gradient_at(0.0);
        for m in 1..200 {
            let g = engine.gradient_at(m as f32);
            // The raw per-point gradient steps by 0.50 (0 → 50%) at the wall;
            // windowing spreads that over the window, so a metre of travel can
            // never move the gradient by more than the wall's rise per window metre.
            assert!(
                (g - prev).abs() <= 0.02,
                "gradient jumped {:.3} at {m} m",
                g - prev
            );
            prev = g;
        }
    }

    #[test]
    fn trainer_grade_ramps_instead_of_stepping() {
        let mut engine = RouteEngine::new(make_step_route(), 0.0, 75.0);
        assert_eq!(engine.trainer_grade_percent(), 0.0);
        // One tick can move the trainer grade by at most 1 percentage point,
        // however steep the road under the rider becomes.
        engine.distance_m = 105.0;
        engine.tick_sim(250);
        assert!(
            engine.trainer_grade_percent() <= 1.0,
            "got {}",
            engine.trainer_grade_percent()
        );
    }

    #[test]
    fn trainer_grade_converges_on_the_road_gradient() {
        let mut engine = RouteEngine::new(make_step_route(), 0.0, 75.0);
        // Hold the rider on the climb and let the ramp catch up.
        for _ in 0..30 {
            engine.distance_m = 105.0;
            engine.tick_sim(250);
        }
        let expected = engine.current_gradient() * 100.0;
        assert!(
            (engine.trainer_grade_percent() - expected).abs() < 0.1,
            "grade {} never reached road gradient {expected}",
            engine.trainer_grade_percent()
        );
    }

    // `set_speed` could be replaced by an empty function body without a single
    // test noticing — the speed the route advances at was only ever set through
    // the constructor.

    #[test]
    fn should_advance_the_route_at_the_speed_it_is_given() {
        let mut engine = RouteEngine::new(make_flat_route(), 5.0, 80.0);
        engine.set_speed(12.0);
        engine.tick();
        assert!(
            (engine.distance_m - 12.0).abs() < 0.01,
            "expected 12 m after a second at 12 m/s, got {}",
            engine.distance_m
        );
    }

    #[test]
    fn should_refuse_to_run_the_route_backwards() {
        // A speed sensor that reports negative, or an ERG override gone wrong,
        // must not wind the rider back down the course.
        let mut engine = RouteEngine::new(make_flat_route(), 5.0, 80.0);
        engine.set_speed(-5.0);
        assert_eq!(engine.speed_ms, 0.0);
        engine.tick();
        assert_eq!(engine.distance_m, 0.0);
    }

    // ── What the map and the ride panel read ────────────────────────────────
    //
    // The tests above drive the engine — tick, gradient, smoothing — and never
    // ask it where the rider is. Mutation testing found the whole accessor
    // surface open: `position_at` could return `None`, or any fixed pair of
    // coordinates, and nothing failed. That is the rider's dot on the map, the
    // distance left, and the elevation under them.

    /// A route that climbs and turns, so an accessor cannot pass by returning
    /// the same number for every position: latitude, longitude and elevation
    /// all differ between the three points.
    fn make_climbing_route() -> Route {
        Route {
            name: "Climbing Test".into(),
            points: vec![
                RoutePoint {
                    lat: 51.0,
                    lng: -0.1,
                    elevation_m: 100.0,
                    distance_m: 0.0,
                    gradient: 0.1,
                },
                RoutePoint {
                    lat: 51.01,
                    lng: -0.11,
                    elevation_m: 150.0,
                    distance_m: 500.0,
                    gradient: -0.1,
                },
                RoutePoint {
                    lat: 51.02,
                    lng: -0.12,
                    elevation_m: 100.0,
                    distance_m: 1000.0,
                    gradient: 0.0,
                },
            ],
            total_distance_m: 1000.0,
            total_gain_m: 50.0,
        }
    }

    #[track_caller]
    fn assert_at(actual: Option<(f64, f64)>, lat: f64, lng: f64) {
        let (a_lat, a_lng) = actual.expect("a route with points has a position");
        assert!(
            (a_lat - lat).abs() < 1e-9 && (a_lng - lng).abs() < 1e-9,
            "expected ({lat}, {lng}), got ({a_lat}, {a_lng})"
        );
    }

    #[test]
    fn should_interpolate_a_position_between_two_route_points() {
        let engine = RouteEngine::new(make_climbing_route(), 5.0, 80.0);
        // A quarter of the way along the first 500 m leg.
        assert_at(engine.position_at(125.0), 51.0025, -0.1025);
        assert_at(engine.position_at(250.0), 51.005, -0.105);
    }

    #[test]
    fn should_sit_on_a_route_point_exactly() {
        let engine = RouteEngine::new(make_climbing_route(), 5.0, 80.0);
        assert_at(engine.position_at(0.0), 51.0, -0.1);
        assert_at(engine.position_at(500.0), 51.01, -0.11);
    }

    #[test]
    fn should_hold_at_the_ends_of_the_route() {
        let engine = RouteEngine::new(make_climbing_route(), 5.0, 80.0);
        // Before the start and past the end, the rider is at the nearest end
        // rather than nowhere — the map still has to draw them.
        assert_at(engine.position_at(-50.0), 51.0, -0.1);
        assert_at(engine.position_at(5000.0), 51.02, -0.12);
    }

    #[test]
    fn should_have_no_position_on_a_route_with_no_points() {
        let empty = Route {
            name: "Empty".into(),
            points: Vec::new(),
            total_distance_m: 0.0,
            total_gain_m: 0.0,
        };
        let engine = RouteEngine::new(empty, 5.0, 80.0);
        assert_eq!(engine.position_at(0.0), None);
        assert_eq!(engine.current_position(), None);
    }

    #[test]
    fn should_report_the_position_the_rider_has_ridden_to() {
        let mut engine = RouteEngine::new(make_climbing_route(), 25.0, 80.0);
        for _ in 0..10 {
            engine.tick();
        }
        assert!((engine.distance_m - 250.0).abs() < 0.01);
        assert_at(engine.current_position(), 51.005, -0.105);
    }

    #[test]
    fn should_report_the_elevation_under_the_rider() {
        let mut engine = RouteEngine::new(make_climbing_route(), 25.0, 80.0);
        let at_start = engine.current_elevation();
        for _ in 0..20 {
            engine.tick();
        }
        // 500 m in, at the top of the climb.
        assert!(
            engine.current_elevation() > at_start,
            "expected to have climbed, went from {at_start} to {}",
            engine.current_elevation()
        );
        assert_eq!(engine.current_elevation(), engine.elevation_at(500.0));
    }

    #[test]
    fn should_count_down_the_distance_left() {
        let mut engine = RouteEngine::new(make_climbing_route(), 25.0, 80.0);
        assert!((engine.remaining_m() - 1000.0).abs() < 0.01);
        for _ in 0..10 {
            engine.tick();
        }
        assert!(
            (engine.remaining_m() - 750.0).abs() < 0.01,
            "expected 750 m left, got {}",
            engine.remaining_m()
        );
    }

    #[test]
    fn should_never_report_a_negative_distance_left() {
        let mut engine = RouteEngine::new(make_climbing_route(), 25.0, 80.0);
        for _ in 0..100 {
            engine.tick();
        }
        assert_eq!(engine.remaining_m(), 0.0);
    }

    #[test]
    fn should_report_progress_as_the_fraction_ridden() {
        let mut engine = RouteEngine::new(make_climbing_route(), 25.0, 80.0);
        assert_eq!(engine.progress(), 0.0);
        for _ in 0..10 {
            engine.tick();
        }
        assert!(
            (engine.progress() - 0.25).abs() < 0.001,
            "expected a quarter, got {}",
            engine.progress()
        );
    }

    #[test]
    fn should_report_no_progress_along_a_route_with_no_length() {
        let empty = Route {
            name: "Nowhere".into(),
            points: Vec::new(),
            total_distance_m: 0.0,
            total_gain_m: 0.0,
        };
        let engine = RouteEngine::new(empty, 5.0, 80.0);
        // Not NaN: this is divided by the route length one line later.
        assert_eq!(engine.progress(), 0.0);
    }

    #[test]
    fn should_read_the_gradient_under_the_rider() {
        let engine = RouteEngine::new(make_climbing_route(), 5.0, 80.0);
        assert_ne!(
            engine.current_gradient(),
            0.0,
            "the route climbs from its first metre"
        );
        assert_eq!(engine.current_gradient(), engine.gradient_at(0.0));
    }

    #[test]
    fn should_ask_the_trainer_for_the_power_the_road_demands() {
        let engine = RouteEngine::new(make_climbing_route(), 8.0, 80.0);
        let expected = Route::power_at(8.0, engine.current_gradient(), 80.0) as u32;
        assert_eq!(engine.current_target_watts(), expected);
        assert!(
            engine.current_target_watts() > 1,
            "8 m/s up a climb is real work, got {} W",
            engine.current_target_watts()
        );
    }

    #[test]
    fn should_count_the_seconds_it_has_simulated() {
        let mut engine = RouteEngine::new(make_climbing_route(), 5.0, 80.0);
        for _ in 0..7 {
            engine.tick();
        }
        assert_eq!(engine.elapsed_secs, 7);
        for _ in 0..3 {
            engine.tick_sim(200);
        }
        assert_eq!(engine.elapsed_secs, 10);
    }

    /// A route carrying two points almost on top of each other, which is what a
    /// GPX with a stationary pause in it looks like.
    fn route_with_doubled_point(second: f32, third: f32) -> Route {
        let at = |lat: f64, lng: f64, distance_m: f32| RoutePoint {
            lat,
            lng,
            elevation_m: 100.0,
            distance_m,
            gradient: 0.0,
        };
        Route {
            name: "Doubled".into(),
            points: vec![
                at(51.0, -0.1, 0.0),
                at(51.5, -0.5, second),
                at(52.0, -0.9, third),
                at(53.0, -1.0, 1000.0),
            ],
            total_distance_m: 1000.0,
            total_gain_m: 0.0,
        }
    }

    #[test]
    fn should_sit_on_the_earlier_point_when_two_share_a_position() {
        // 5 cm apart: there is no meaningful distance to interpolate along, and
        // dividing by it would be dividing by nearly nothing.
        let engine = RouteEngine::new(route_with_doubled_point(500.0, 500.05), 5.0, 80.0);
        assert_at(engine.position_at(500.02), 51.5, -0.5);
    }

    #[test]
    fn should_still_interpolate_across_a_span_the_guard_allows() {
        // Exactly the guard's width: the rule is "closer than", so 0.1 m is
        // still a span to interpolate along, and half way along it is half way.
        let engine = RouteEngine::new(route_with_doubled_point(0.1, 1000.0), 5.0, 80.0);
        assert_at(engine.position_at(0.05), 51.25, -0.3);
    }
}
