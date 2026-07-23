#![allow(dead_code)]

use crate::data::route::Route;

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

    /// Target power (W) at the current position and speed.
    pub fn current_target_watts(&self) -> u32 {
        Route::power_at(self.speed_ms, self.current_gradient(), self.mass_kg) as u32
    }

    /// Advance one simulated second. Returns target watts for that second.
    pub fn tick(&mut self) -> u32 {
        let watts = self.current_target_watts();
        self.distance_m = (self.distance_m + self.speed_ms).min(self.route.total_distance_m);
        self.elapsed_secs += 1;
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
        speed
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

    /// Elevation at `distance_m` by linear interpolation between route points.
    pub fn elevation_at(&self, distance_m: f32) -> f32 {
        let pts = &self.route.points;
        if pts.is_empty() {
            return 0.0;
        }
        let idx = pts.partition_point(|p| p.distance_m <= distance_m);
        if idx == 0 {
            return pts[0].elevation_m;
        }
        if idx >= pts.len() {
            return pts[pts.len() - 1].elevation_m;
        }
        let p0 = &pts[idx - 1];
        let p1 = &pts[idx];
        let span = p1.distance_m - p0.distance_m;
        if span < 0.1 {
            return p0.elevation_m;
        }
        let t = (distance_m - p0.distance_m) / span;
        p0.elevation_m + t * (p1.elevation_m - p0.elevation_m)
    }

    fn gradient_at(&self, distance_m: f32) -> f32 {
        let pts = &self.route.points;
        if pts.is_empty() {
            return 0.0;
        }
        let idx = pts.partition_point(|p| p.distance_m <= distance_m);
        let idx = idx.saturating_sub(1).min(pts.len() - 1);
        pts[idx].gradient
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
}
