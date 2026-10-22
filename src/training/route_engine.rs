#![allow(dead_code)]

use crate::data::bends;
use crate::data::route::Route;

/// Maximum change in the grade sent to the trainer, in percentage points per second.
///
/// Real roads ramp into their gradient; stepping the trainer straight to a new
/// grade makes it lurch. At 1 %/s a 0 → 8 % wall arrives over 8 seconds.
const MAX_GRADE_SLEW_PCT_PER_S: f32 = 1.0;

/// The grade (%) actually sent to the trainer in SIM mode: the rate-limited road
/// grade scaled by the rider's difficulty setting and live dial, and capped at
/// their maximum gradient.
///
/// The settings come back from the database unvalidated, and `f32::clamp`
/// panics on backwards or NaN bounds — which would end the ride on its first
/// tick. A negative cap is read as its size, a NaN one as the default, and a
/// NaN grade as a flat road: the one grade that is never wrong to send.
pub fn grade_for_trainer(
    trainer_grade_pct: f32,
    difficulty: f32,
    dial: f32,
    max_grade_pct: f32,
) -> f32 {
    let cap = if max_grade_pct.is_nan() {
        crate::data::settings::TrainingSettings::default().sim_max_gradient_pct
    } else {
        max_grade_pct.abs()
    };
    let grade = trainer_grade_pct * difficulty * dial;
    if grade.is_nan() {
        return 0.0;
    }
    grade.clamp(-cap, cap)
}

/// The command that lets the trainer go when a route ride pauses or ends — a
/// flat road in SIM, no target in ERG emulation. A trainer holds whatever it
/// was last told, so without this the rider steps off a climb still set.
pub fn release_command(sim: bool) -> crate::devices::manager::DeviceCommand {
    use crate::devices::manager::DeviceCommand;
    if sim {
        DeviceCommand::SetSimulation { grade_percent: 0.0 }
    } else {
        DeviceCommand::SetTargetPower { watts: 0 }
    }
}

/// Fastest the virtual rider will ride, in m/s (55 km/h).
///
/// The physics alone lets a rider free-wheel a −8 % road at 65 km/h and a
/// −10 % one at 73, and nobody rides an open city descent like that. Measured
/// on the rider's own 35 outdoor rides: their fastest moment on a descent is
/// typically 45 km/h, 53 on a quick day, 63 at the very most. 55 is the quick
/// day — fast enough to feel, never faster than they actually go.
pub const MAX_SPEED_MS: f32 = 55.0 / 3.6;

/// Sideways acceleration the virtual rider will take through a bend, m/s².
/// The rider's own descents put this at 0.6 typical, 1.5 at the 90th
/// percentile and 2.25 at the 98th; 2.0 slows them only for real bends.
pub const MAX_LATERAL_ACCEL: f32 = 2.0;

/// How hard the virtual rider brakes for a bend ahead, m/s² — about a third of
/// a g, firm but unhurried.
pub const BRAKING_DECEL: f32 = 3.0;

/// Steps each simulated second is integrated in. A tenth of a second keeps the
/// speed honest through a change of gradient without costing anything.
const SUBSTEPS: u32 = 10;

/// Floor under the speed the pedal force is worked out at, in m/s. Force is
/// power over speed, which is unbounded from a standstill.
const MIN_PROPULSION_SPEED_MS: f32 = 1.0;

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
    /// The fastest the virtual rider will go at each route point — see
    /// [`RouteEngine::speed_limit_at`]. Worked out once, when the route loads.
    speed_limits: Vec<f32>,
}

impl RouteEngine {
    pub fn new(route: Route, speed_ms: f32, mass_kg: f32) -> Self {
        let speed_limits = speed_limits(&route);
        Self {
            speed_limits,
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

    /// The fastest the virtual rider will be going at `distance_m`: the
    /// ceiling, a bend, or braking for a bend ahead.
    ///
    /// Between two route points the lower of their limits applies, so the
    /// rider is never carried into a slower stretch faster than it allows.
    pub fn speed_limit_at(&self, distance_m: f32) -> f32 {
        let pts = &self.route.points;
        if self.speed_limits.len() != pts.len() || pts.is_empty() {
            return MAX_SPEED_MS;
        }
        let idx = pts.partition_point(|p| p.distance_m <= distance_m);
        let before = self.speed_limits[idx.saturating_sub(1)];
        let after = self.speed_limits.get(idx).copied().unwrap_or(before);
        before.min(after)
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
    ///
    /// Speed carries momentum: each tenth of a second it changes by the pedal
    /// force less gravity, rolling and air resistance, over the mass — so a
    /// descent is built into over seconds and run out of onto the flat, as on a
    /// real bike. It is then held to [`RouteEngine::speed_limit_at`]: the
    /// virtual rider brakes for bends and never rides faster than the rider
    /// does outdoors. On a long straight the speed settles exactly where
    /// [`Route::speed_from_power`] puts it, below the ceiling.
    pub fn tick_sim(&mut self, power_watts: u32) -> f32 {
        const ALPHA: f32 = 0.3; // EMA weight ≈ 3-second smoothing at 1 Hz
        self.smoothed_power = self.smoothed_power * (1.0 - ALPHA) + power_watts as f32 * ALPHA;

        // Weight is read from the database; one that is zero or garbage would
        // divide the ride into NaN, so it falls back to the profile default.
        let mass = if self.mass_kg.is_finite() && self.mass_kg > 0.0 {
            self.mass_kg
        } else {
            crate::data::athlete::AthleteProfile::default().weight_kg
        };
        let end = self.route.total_distance_m;
        let dt = 1.0 / SUBSTEPS as f32;
        let mut v = if self.speed_ms.is_finite() {
            self.speed_ms.max(0.0)
        } else {
            0.0
        };
        let mut d = self.distance_m;
        for _ in 0..SUBSTEPS {
            let drive = self.smoothed_power / v.max(MIN_PROPULSION_SPEED_MS);
            let resist = Route::resistance_n(v, self.gradient_at(d), mass);
            // Never below a standstill: with no drive, rolling resistance and
            // a climb hold the bike still rather than run it backwards.
            v = (v + (drive - resist) / mass * dt).max(0.0);
            v = v.min(self.speed_limit_at(d));
            d = (d + v * dt).min(end);
        }
        self.speed_ms = v;
        self.distance_m = d;
        self.elapsed_secs += 1;
        self.slew_trainer_grade();
        v
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

    /// Whether this tick's speed is the rider's own doing: SIM physics turned
    /// their power into it, a speed sensor measured it, or they are standing
    /// still. Not when ERG emulation moves them at the nominal pace — a ride
    /// with any such tick is not a time to race.
    pub fn rider_sets_speed(sim: bool, power_watts: Option<u32>, speed_kmh: Option<f32>) -> bool {
        sim || speed_kmh.is_some() || power_watts.unwrap_or(0) == 0
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

/// The fastest the virtual rider will go at each point of `route`.
///
/// Each point is limited by the ceiling and by the bend it sits in — the speed
/// that keeps sideways acceleration at [`MAX_LATERAL_ACCEL`] round the radius
/// measured there. A pass from the finish back then lowers every point to
/// what braking at [`BRAKING_DECEL`] from there leaves room for, so the rider
/// slows *before* a bend rather than in it.
///
/// Bends are measured on the GPX alone ([`bends::curvatures`]) — never on
/// anything the 3D view draws, so restyling the scene can never change how
/// fast a descent rides (stylized-route skill, rule 5). The view reads the
/// same measurement to lay its road through the same bends.
fn speed_limits(route: &Route) -> Vec<f32> {
    let mut limits: Vec<f32> = bends::curvatures(route)
        .into_iter()
        .map(|k| {
            if k == 0.0 {
                MAX_SPEED_MS
            } else {
                (MAX_LATERAL_ACCEL / k.abs()).sqrt().min(MAX_SPEED_MS)
            }
        })
        .collect();
    let pts = &route.points;
    for i in (0..limits.len().saturating_sub(1)).rev() {
        let gap = (pts[i + 1].distance_m - pts[i].distance_m).max(0.0);
        let reach = (limits[i + 1].powi(2) + 2.0 * BRAKING_DECEL * gap).sqrt();
        limits[i] = limits[i].min(reach);
    }
    limits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::route::RoutePoint;

    // ── The grade the trainer is sent ───────────────────────────────────────
    // Both settings feeding this are read back from the database unvalidated,
    // and `f32::clamp` panics when its bounds are backwards or NaN — on the
    // first tick of a route ride, losing it.

    #[test]
    fn should_release_a_sim_trainer_onto_a_flat_road() {
        use crate::devices::manager::DeviceCommand;
        assert!(matches!(
            release_command(true),
            DeviceCommand::SetSimulation { grade_percent } if grade_percent == 0.0
        ));
    }

    #[test]
    fn should_release_an_erg_trainer_to_no_target() {
        use crate::devices::manager::DeviceCommand;
        assert!(matches!(
            release_command(false),
            DeviceCommand::SetTargetPower { watts: 0 }
        ));
    }

    #[test]
    fn should_cap_a_climb_at_the_max_gradient() {
        assert_eq!(grade_for_trainer(25.0, 1.0, 1.0, 20.0), 20.0);
    }

    #[test]
    fn should_cap_a_descent_at_the_max_gradient() {
        assert_eq!(grade_for_trainer(-25.0, 1.0, 1.0, 20.0), -20.0);
    }

    #[test]
    fn should_scale_the_grade_by_difficulty_and_dial() {
        assert_eq!(grade_for_trainer(8.0, 0.5, 0.9, 20.0), 3.6);
    }

    #[test]
    fn should_read_a_negative_max_gradient_as_its_size() {
        assert_eq!(grade_for_trainer(8.0, 1.0, 1.0, -5.0), 5.0);
    }

    #[test]
    fn should_fall_back_to_the_default_cap_when_the_max_gradient_is_nan() {
        assert_eq!(grade_for_trainer(25.0, 1.0, 1.0, f32::NAN), 20.0);
    }

    #[test]
    fn should_send_a_flat_road_when_the_difficulty_is_nan() {
        assert_eq!(grade_for_trainer(8.0, f32::NAN, 1.0, 20.0), 0.0);
    }

    #[test]
    fn should_send_a_flat_road_when_the_max_gradient_is_zero() {
        assert_eq!(grade_for_trainer(8.0, 1.0, 1.0, 0.0), 0.0);
    }

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
        // From a standstill a bike takes time to get going. Ten seconds of a
        // power that is still ramping up through the smoothing is ~1.6 kJ, and
        // 75 kg at 6 m/s already holds 1.35 kJ: about 6 m/s and 30 m, not the
        // 9.4 m/s it settles at.
        assert!(engine.distance_m > 25.0, "got {} m", engine.distance_m);
        assert!(engine.speed_ms > 5.0, "got {} m/s", engine.speed_ms);
        for _ in 0..50 {
            engine.tick_sim(200);
        }
        assert!(
            engine.speed_ms > 9.0,
            "never got up to speed: {} m/s",
            engine.speed_ms
        );
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

    // ── Momentum and braking ─────────────────────────────────────────────────

    /// A road laid out in metres from (41.0, 29.0) — Istanbul, where the
    /// rider's own descents were measured — from `(east, north, elevation)`.
    fn road(points: &[(f64, f64, f32)]) -> Route {
        let frame = crate::data::course3d::LocalFrame::at(41.0, 29.0);
        let mut distance = 0.0f32;
        let mut out: Vec<RoutePoint> = Vec::new();
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
        Route {
            name: "Road".into(),
            total_distance_m: distance,
            total_gain_m: 0.0,
            points: out,
        }
    }

    /// Due north for `flat_m` on the level, then `descent_m` falling at
    /// `grade`, then `flat_after_m` level again. A point every 8 m, as recorded.
    fn straight(flat_m: f64, descent_m: f64, grade: f32, flat_after_m: f64) -> Route {
        let total = flat_m + descent_m + flat_after_m;
        let pts: Vec<(f64, f64, f32)> = (0..=(total / 8.0) as usize)
            .map(|i| {
                let n = i as f64 * 8.0;
                let down = (n - flat_m).clamp(0.0, descent_m) as f32;
                (0.0, n, 500.0 + grade * down)
            })
            .collect();
        road(&pts)
    }

    /// Run `secs` SIM seconds at `watts`, returning the fastest speed seen.
    fn ride(engine: &mut RouteEngine, secs: u32, watts: u32) -> f32 {
        let mut top = 0.0f32;
        for _ in 0..secs {
            top = top.max(engine.tick_sim(watts));
        }
        top
    }

    #[test]
    fn should_cover_about_the_distance_its_recorded_speeds_add_up_to() {
        // The ghost rider is rebuilt from a ride's recorded speed, one value a
        // second, while the engine moves in smaller steps within each second.
        // Over a ride with surges, a descent and a stop, the two must agree
        // closely enough for the ghost to be scaled onto the route's length.
        let mut engine = RouteEngine::new(straight(2_000.0, 1_500.0, -0.06, 2_000.0), 0.0, 75.0);
        let mut summed = 0.0f32;
        for k in 0..900u32 {
            let watts = match k % 120 {
                0..=29 => 420,
                30..=59 => 0,
                _ => 210,
            };
            summed += engine.tick_sim(watts);
            if engine.is_done() {
                break;
            }
        }
        let rode = engine.distance_m;
        assert!(rode > 3_000.0, "rode only {rode} m");
        let off = (summed - rode).abs() / rode;
        assert!(
            off < 0.02,
            "speeds add up to {summed} m for {rode} m ridden ({off:.3})"
        );
    }

    #[test]
    fn should_settle_at_the_steady_state_speed_on_the_flat() {
        // Momentum changes how speed is reached, never where it ends up.
        let mut engine = RouteEngine::new(straight(20_000.0, 0.0, 0.0, 0.0), 0.0, 75.0);
        ride(&mut engine, 180, 250);
        let steady = Route::speed_from_power(250.0, 0.0, 75.0);
        assert!(
            (engine.speed_ms - steady).abs() / steady < 0.005,
            "{} vs steady {steady}",
            engine.speed_ms
        );
    }

    #[test]
    fn should_build_speed_over_seconds_into_a_descent() {
        let mut engine = RouteEngine::new(straight(1_000.0, 3_000.0, -0.08, 0.0), 0.0, 75.0);
        ride(&mut engine, 120, 150);
        let flat = engine.speed_ms;
        // Put the rider just past the crest, at their flat speed.
        engine.distance_m = 1_030.0;
        engine.tick_sim(150);
        let steady = Route::speed_from_power(150.0, -0.08, 75.0).min(MAX_SPEED_MS);
        assert!(engine.speed_ms > flat, "gravity must pull at once");
        assert!(
            engine.speed_ms < flat + 1.5,
            "one second gained {} m/s — no bike accelerates like that",
            engine.speed_ms - flat
        );
        ride(&mut engine, 40, 150);
        assert!(
            (engine.speed_ms - steady).abs() < 0.05,
            "{} vs {steady}",
            engine.speed_ms
        );
    }

    #[test]
    fn should_hold_exactly_the_ceiling_on_a_long_steep_straight() {
        let mut engine = RouteEngine::new(straight(0.0, 5_000.0, -0.12, 0.0), 0.0, 75.0);
        let top = ride(&mut engine, 200, 400);
        assert!(top <= MAX_SPEED_MS + 1e-4, "{} km/h", top * 3.6);
        assert!(
            top >= MAX_SPEED_MS - 1e-3,
            "never reached the ceiling: {} km/h",
            top * 3.6
        );
    }

    #[test]
    fn should_survive_a_rogue_power_reading() {
        let mut engine = RouteEngine::new(straight(5_000.0, 0.0, 0.0, 0.0), 0.0, 75.0);
        let top = ride(&mut engine, 10, 65_535);
        assert!(top.is_finite() && top <= MAX_SPEED_MS + 1e-4, "{top}");
    }

    #[test]
    fn should_carry_speed_off_a_descent_onto_the_flat() {
        let mut engine = RouteEngine::new(straight(0.0, 2_000.0, -0.08, 3_000.0), 0.0, 75.0);
        // Free-wheel down to near the bottom, at whatever speed that gives.
        for _ in 0..1_000 {
            if engine.distance_m >= 1_900.0 {
                break;
            }
            engine.tick_sim(0);
        }
        assert!(
            engine.speed_ms > 12.0,
            "a −8 % road is fast: {}",
            engine.speed_ms
        );
        // Free-wheeling on the flat: the old model stopped dead in one second.
        engine.distance_m = 2_100.0;
        engine.tick_sim(0);
        assert!(
            engine.speed_ms > 10.0,
            "lost all speed at once: {}",
            engine.speed_ms
        );
    }

    #[test]
    fn should_roll_to_a_stop_without_power_and_never_backwards() {
        let mut engine = RouteEngine::new(straight(0.0, 0.0, 0.0, 10_000.0), 10.0, 75.0);
        let mut last = engine.distance_m;
        for _ in 0..900 {
            let v = engine.tick_sim(0);
            assert!(v >= 0.0 && v.is_finite());
            assert!(engine.distance_m >= last);
            last = engine.distance_m;
        }
        assert_eq!(engine.speed_ms, 0.0);
    }

    #[test]
    fn should_not_roll_back_down_a_climb() {
        let mut engine = RouteEngine::new(straight(0.0, 2_000.0, 0.08, 0.0), 5.0, 75.0);
        engine.distance_m = 500.0;
        for _ in 0..30 {
            engine.tick_sim(0);
            assert!(engine.distance_m >= 500.0);
        }
        assert_eq!(engine.speed_ms, 0.0);
    }

    /// Due north down a −6 % road, a right-hand bend of `radius` metres
    /// through 90°, then due east. Returns the route and the bend's middle,
    /// in metres from the start.
    fn bend(radius: f64) -> (Route, f32) {
        let approach = 800.0;
        let mut pts: Vec<(f64, f64, f32)> = Vec::new();
        let mut along = 0.0f64;
        let ele = |d: f64| 500.0 - 0.06 * d as f32;
        for i in 0..=(approach / 8.0) as usize {
            along = i as f64 * 8.0;
            pts.push((0.0, along, ele(along)));
        }
        // Centre of the bend lies east of the approach.
        let arc = radius * std::f64::consts::FRAC_PI_2;
        let steps = (arc / 4.0).ceil() as usize;
        for k in 1..=steps {
            let t = k as f64 / steps as f64 * std::f64::consts::FRAC_PI_2;
            let (e, n) = (radius - radius * t.cos(), approach + radius * t.sin());
            pts.push((e, n, ele(approach + arc * k as f64 / steps as f64)));
        }
        let end = approach + arc;
        for i in 1..=60 {
            let d = end + i as f64 * 8.0;
            pts.push((radius + i as f64 * 8.0, approach + radius, ele(d)));
        }
        let _ = along;
        (road(&pts), (approach + arc / 2.0) as f32)
    }

    #[test]
    fn should_not_produce_nan_from_a_profile_with_no_weight() {
        // Weight comes from the database; a zero or garbage value must not
        // turn the rider's position into NaN for the rest of the ride.
        for mass in [0.0, -70.0, f32::NAN] {
            let mut engine = RouteEngine::new(straight(0.0, 2_000.0, -0.06, 0.0), 0.0, mass);
            ride(&mut engine, 20, 200);
            assert!(
                engine.distance_m.is_finite() && engine.speed_ms.is_finite(),
                "{mass}"
            );
            assert!(engine.distance_m > 0.0, "{mass}: never moved");
        }
    }

    #[test]
    fn should_slow_for_a_bend_to_what_the_rider_takes_one_at() {
        let (route, apex) = bend(30.0);
        let engine = RouteEngine::new(route, 0.0, 75.0);
        // The rider's own 98th percentile through a 30 m bend: ~2.25 m/s².
        // A small allowance for the chord reading a tight bend a little wide.
        let limit = engine.speed_limit_at(apex);
        assert!(
            limit <= (MAX_LATERAL_ACCEL * 30.0 * 1.25).sqrt(),
            "{} km/h",
            limit * 3.6
        );
        assert!(
            limit >= (MAX_LATERAL_ACCEL * 30.0 * 0.8).sqrt(),
            "{} km/h",
            limit * 3.6
        );
    }

    #[test]
    fn should_brake_before_the_bend_not_in_it() {
        let (route, apex) = bend(30.0);
        let engine = RouteEngine::new(route, 0.0, 75.0);
        let at_apex = engine.speed_limit_at(apex);
        // 60 m before, the limit is what braking at BRAKING_DECEL leaves room
        // for, not the ceiling — and not yet the bend speed either.
        let before = engine.speed_limit_at(apex - 60.0);
        let envelope = (at_apex * at_apex + 2.0 * BRAKING_DECEL * 60.0).sqrt();
        assert!(before > at_apex + 1.0, "{} vs apex {}", before, at_apex);
        assert!(
            before <= envelope + 0.5,
            "{} vs envelope {envelope}",
            before
        );
    }

    #[test]
    fn should_actually_ride_the_bend_at_the_limit() {
        let (route, apex) = bend(30.0);
        let mut engine = RouteEngine::new(route, 0.0, 75.0);
        let limit = engine.speed_limit_at(apex);
        let mut at_apex = None;
        for _ in 0..400 {
            engine.tick_sim(250);
            if at_apex.is_none() && engine.distance_m >= apex {
                at_apex = Some(engine.speed_ms);
            }
        }
        let v = at_apex.expect("rode past the bend");
        assert!(v <= limit + 0.3, "took the bend at {} km/h", v * 3.6);
    }

    #[test]
    fn should_not_brake_on_a_straight_road_with_gps_scatter() {
        // A straight −6 % descent recorded with a metre and a half of
        // side-to-side scatter: no bend, so nothing below the ceiling.
        let pts: Vec<(f64, f64, f32)> = (0..300)
            .map(|i| {
                let n = i as f64 * 8.0;
                // Deterministic scatter, never the same twice in a row.
                let e = 1.5 * ((i * 7919 % 13) as f64 / 6.0 - 1.0);
                (e, n, 500.0 - 0.06 * n as f32)
            })
            .collect();
        let route = road(&pts);
        let total = route.total_distance_m;
        let engine = RouteEngine::new(route, 0.0, 75.0);
        let lowest = (0..total as u32)
            .map(|d| engine.speed_limit_at(d as f32))
            .fold(f32::MAX, f32::min);
        assert!(
            lowest >= 50.0 / 3.6,
            "braked to {} km/h on a straight",
            lowest * 3.6
        );
    }

    #[test]
    fn should_treat_a_route_with_no_shape_as_straight() {
        // Every point at one latitude and longitude, as several test routes and
        // a GPX of a stationary trainer ride are.
        let engine = RouteEngine::new(make_step_route(), 0.0, 75.0);
        assert_eq!(engine.speed_limit_at(105.0), MAX_SPEED_MS);
        let empty = Route {
            name: "Empty".into(),
            points: Vec::new(),
            total_distance_m: 0.0,
            total_gain_m: 0.0,
        };
        assert_eq!(
            RouteEngine::new(empty, 0.0, 75.0).speed_limit_at(0.0),
            MAX_SPEED_MS
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
    fn should_say_the_rider_set_the_speed_only_when_they_did() {
        // SIM: the physics turns the rider's power into speed.
        assert!(RouteEngine::rider_sets_speed(true, Some(200), None));
        // A speed sensor: what the wheel did.
        assert!(RouteEngine::rider_sets_speed(false, Some(200), Some(31.0)));
        // Standing still sets nothing either way.
        assert!(RouteEngine::rider_sets_speed(false, None, None));
        assert!(RouteEngine::rider_sets_speed(false, Some(0), None));
        // ERG emulation: the clock moved them at the nominal pace.
        assert!(!RouteEngine::rider_sets_speed(false, Some(1), None));
        assert!(!RouteEngine::rider_sets_speed(false, Some(400), None));
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
