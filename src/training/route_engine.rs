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
}
