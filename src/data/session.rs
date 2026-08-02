use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

use super::workout::Segment;

/// A completed or in-progress training session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: i64,
    pub workout_id: Option<i64>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub data_points: Vec<DataPoint>,
    /// Rate of Perceived Exertion (1–10), recorded after the session ends.
    #[serde(default)]
    pub rpe: Option<u8>,
    /// The athlete's FTP at ride time — required by FTP detection to interpret
    /// compliance and zones after the profile FTP changes (docs/ftp-detection.md).
    #[serde(default)]
    pub ftp_watts: Option<u32>,
    /// Display name for the activity — the route name for a GPX ride, or a name
    /// the rider typed. `None` falls back to the linked workout's name.
    #[serde(default)]
    pub title: Option<String>,
    /// The Intervals.icu activity this ride is the same event as, once the two
    /// have been matched. Set when the ride reaches Intervals.icu by any route —
    /// uploaded by the app, or synced in from Garmin or Strava — so the ride is
    /// shown and counted once. See [`crate::data::dedupe`].
    #[serde(default)]
    pub icu_id: Option<String>,
}

impl Session {
    pub fn new(workout_id: Option<i64>) -> Self {
        Self {
            id: 0,
            workout_id,
            started_at: Utc::now(),
            ended_at: None,
            data_points: Vec::new(),
            rpe: None,
            ftp_watts: None,
            title: None,
            icu_id: None,
        }
    }

    #[allow(dead_code)]
    pub fn duration_secs(&self) -> u64 {
        let end = self.ended_at.unwrap_or_else(Utc::now);
        (end - self.started_at).num_seconds().max(0) as u64
    }

    #[allow(dead_code)]
    pub fn kilojoules(&self) -> f32 {
        let avg_watts = self.average_power().unwrap_or(0.0);
        avg_watts * self.duration_secs() as f32 / 1000.0
    }

    #[allow(dead_code)]
    pub fn average_power(&self) -> Option<f32> {
        let readings: Vec<f32> = self
            .data_points
            .iter()
            .filter_map(|p| p.power_watts)
            .map(|w| w as f32)
            .collect();
        if readings.is_empty() {
            None
        } else {
            Some(readings.iter().sum::<f32>() / readings.len() as f32)
        }
    }

    /// Mean of a per-second field over the points where it was recorded.
    fn average_of(&self, f: impl Fn(&DataPoint) -> Option<u32>) -> Option<f32> {
        let values: Vec<f32> = self
            .data_points
            .iter()
            .filter_map(&f)
            .map(|v| v as f32)
            .collect();
        if values.is_empty() {
            None
        } else {
            Some(values.iter().sum::<f32>() / values.len() as f32)
        }
    }

    /// Average heart rate in bpm, over the seconds a monitor was reporting.
    pub fn average_hr(&self) -> Option<f32> {
        self.average_of(|p| p.heart_rate_bpm)
    }

    /// Peak heart rate in bpm.
    pub fn max_hr(&self) -> Option<u32> {
        self.data_points
            .iter()
            .filter_map(|p| p.heart_rate_bpm)
            .max()
    }

    /// Average cadence in rpm, counting only the seconds the rider was pedalling —
    /// coasting zeros would otherwise drag the figure down misleadingly.
    pub fn average_cadence(&self) -> Option<f32> {
        self.average_of(|p| p.cadence_rpm.filter(|&c| c > 0))
    }

    /// Peak power in watts.
    pub fn max_power(&self) -> Option<u32> {
        self.data_points.iter().filter_map(|p| p.power_watts).max()
    }

    /// Distance covered in metres, integrating the per-second speed.
    pub fn distance_m(&self) -> f32 {
        self.data_points
            .iter()
            .filter_map(|p| p.speed_kmh)
            .map(|kmh| kmh / 3.6)
            .sum()
    }

    /// Total elevation gain in metres — the sum of the positive altitude changes.
    ///
    /// Rises below `MIN_CLIMB_STEP_M` are treated as noise and ignored, so a
    /// jittery altitude trace does not accumulate phantom metres.
    pub fn elevation_gain_m(&self) -> Option<f32> {
        const MIN_CLIMB_STEP_M: f32 = 0.5;
        let alts: Vec<f32> = self
            .data_points
            .iter()
            .filter_map(|p| p.altitude_m)
            .collect();
        if alts.len() < 2 {
            return None;
        }
        let mut gain = 0.0;
        let mut reference = alts[0];
        for &a in &alts[1..] {
            let delta = a - reference;
            if delta >= MIN_CLIMB_STEP_M {
                gain += delta;
                reference = a;
            } else if delta < 0.0 {
                reference = a;
            }
        }
        Some(gain)
    }

    /// Seconds spent in each Coggan power zone Z1–Z7, indexed Z1 = 0.
    pub fn time_in_zones(&self, ftp: u32) -> [u32; 7] {
        let mut zones = [0u32; 7];
        for p in &self.data_points {
            if let Some(w) = p.power_watts {
                zones[crate::data::athlete::power_zone_index(w, ftp)] += 1;
            }
        }
        zones
    }

    /// Normalised Power — 30s rolling average → 4th power → mean → 4th root.
    #[allow(dead_code)]
    pub fn normalised_power(&self) -> Option<f32> {
        let powers: Vec<f32> = self
            .data_points
            .iter()
            .filter_map(|p| p.power_watts)
            .map(|w| w as f32)
            .collect();
        if powers.len() < 30 {
            return None;
        }
        let rolling: Vec<f32> = powers
            .windows(30)
            .map(|w| w.iter().sum::<f32>() / 30.0)
            .collect();
        let mean_fourth: f32 =
            rolling.iter().map(|p| p.powi(4)).sum::<f32>() / rolling.len() as f32;
        Some(mean_fourth.powf(0.25))
    }

    /// Peak average power over a rolling window of `duration_secs` consecutive data points.
    pub fn peak_power_for_duration(&self, duration_secs: usize) -> Option<u32> {
        let powers: Vec<u32> = self
            .data_points
            .iter()
            .filter_map(|p| p.power_watts)
            .collect();
        if powers.len() < duration_secs {
            return None;
        }
        powers
            .windows(duration_secs)
            .map(|w| w.iter().sum::<u32>() / duration_secs as u32)
            .max()
    }

    /// Training Stress Score.
    #[allow(dead_code)]
    pub fn tss(&self, ftp: u32) -> Option<f32> {
        let np = self.normalised_power()?;
        let if_ = np / ftp as f32;
        let hours = self.duration_secs() as f32 / 3600.0;
        Some(if_.powi(2) * hours * 100.0)
    }
}

/// One data point recorded approximately once per second.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataPoint {
    pub elapsed_secs: u32,
    pub power_watts: Option<u32>,
    /// The workout target in force at this second (`None` outside structured
    /// workouts, e.g. route rides and imported files). Recorded for FTP
    /// detection — see docs/ftp-detection.md.
    #[serde(default)]
    pub target_watts: Option<u32>,
    pub heart_rate_bpm: Option<u32>,
    pub cadence_rpm: Option<u32>,
    pub speed_kmh: Option<f32>,
    /// WGS-84 latitude in degrees. Present only for GPS activities.
    #[serde(default)]
    pub lat: Option<f64>,
    /// WGS-84 longitude in degrees. Present only for GPS activities.
    #[serde(default)]
    pub lng: Option<f64>,
    /// Altitude in metres above sea level. Present only for GPS activities.
    #[serde(default)]
    pub altitude_m: Option<f32>,
}

/// Per-segment stats computed from session data vs. the structured workout plan.
#[derive(Debug, Clone)]
pub struct IntervalStats {
    pub label: String,
    /// Mid-point target watts (average of power_low and power_high at FTP).
    pub target_watts: u32,
    /// Measured average watts for the segment (None if no power data).
    pub avg_watts: Option<f32>,
    /// Seconds within ±10 % of target (out of the segment's duration).
    pub seconds_on_target: u32,
    pub duration_secs: u32,
    /// Segment has a target above 55 % FTP and is counted for compliance.
    pub is_active: bool,
}

impl Session {
    /// Per-segment stats against the structured workout plan.
    pub fn interval_analysis(&self, segments: &[Segment], ftp: u32) -> Vec<IntervalStats> {
        let mut stats = Vec::with_capacity(segments.len());
        let mut seg_start = 0u32;

        for (i, seg) in segments.iter().enumerate() {
            let seg_end = seg_start + seg.duration_secs;
            let mid_pct = (seg.power_low_pct + seg.power_high_pct) / 2.0;
            let target_watts = (mid_pct / 100.0 * ftp as f32) as u32;
            let is_active = mid_pct > 55.0;

            let powers: Vec<u32> = self
                .data_points
                .iter()
                .filter(|dp| dp.elapsed_secs >= seg_start && dp.elapsed_secs < seg_end)
                .filter_map(|dp| dp.power_watts)
                .collect();

            let avg_watts = if powers.is_empty() {
                None
            } else {
                Some(powers.iter().sum::<u32>() as f32 / powers.len() as f32)
            };

            let seconds_on_target = if target_watts == 0 {
                0
            } else {
                let lo = (target_watts as f32 * 0.90) as u32;
                let hi = (target_watts as f32 * 1.10) as u32;
                powers.iter().filter(|&&w| w >= lo && w <= hi).count() as u32
            };

            stats.push(IntervalStats {
                label: seg
                    .label
                    .clone()
                    .unwrap_or_else(|| format!("Segment {}", i + 1)),
                target_watts,
                avg_watts,
                seconds_on_target,
                duration_secs: seg.duration_secs,
                is_active,
            });

            seg_start = seg_end;
        }

        stats
    }

    /// Percentage of active-segment seconds that were within ±10 % of target power.
    /// Returns `None` when the workout has no active segments or no power data.
    pub fn compliance_pct(&self, segments: &[Segment], ftp: u32) -> Option<u8> {
        let stats = self.interval_analysis(segments, ftp);
        let active: Vec<&IntervalStats> = stats.iter().filter(|s| s.is_active).collect();
        if active.is_empty() {
            return None;
        }
        let total: u32 = active.iter().map(|s| s.duration_secs).sum();
        if total == 0 {
            return None;
        }
        let on_target: u32 = active.iter().map(|s| s.seconds_on_target).sum();
        Some(((on_target * 100) / total).min(100) as u8)
    }
}

/// Real-time sensor readings broadcast from the Device Manager to the UI.
#[derive(Debug, Clone, Default)]
pub struct LiveReadings {
    pub power_watts: Option<u32>,
    pub heart_rate_bpm: Option<u32>,
    pub cadence_rpm: Option<u32>,
    pub speed_kmh: Option<f32>,
    #[allow(dead_code)]
    pub resistance_target_watts: Option<u32>,
    /// Which radio carried this reading. Defaults to BLE, so only ANT+ sources
    /// need to say so.
    pub source: ReadingSource,
}

/// Which radio a reading arrived over.
///
/// ANT+ is preferred wherever it is available: it is a broadcast protocol with no
/// connection to lose, so it does not suffer the notification stalls BLE is prone
/// to on a busy adapter. Anything not explicitly ANT+ is treated as the fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReadingSource {
    /// Bluetooth LE, or a source that did not say.
    #[default]
    Ble,
    Ant,
}

/// How long a sensor value stays usable after it last arrived.
///
/// Heart-rate straps and trainers transmit at roughly 1 Hz, so ten seconds of
/// silence means the sensor has stopped — not that the rider's heart rate has
/// been perfectly constant.
pub const SENSOR_TIMEOUT: Duration = Duration::from_secs(10);

/// The latest value from each sensor, with the time it arrived.
///
/// Sensors report independently and at different rates, so readings have to be
/// merged field by field rather than wholesale. But a merged value must not
/// outlive its sensor: a strap that drops mid-ride would otherwise have its last
/// reading displayed and recorded once a second for the rest of the ride,
/// fabricating a flat trace that never happened. Each field is therefore kept
/// with its arrival time and expires after [`SENSOR_TIMEOUT`].
#[derive(Debug, Default, Clone)]
pub struct ReadingsTracker {
    power: Option<Sample<u32>>,
    heart_rate: Option<Sample<u32>>,
    cadence: Option<Sample<u32>>,
    speed: Option<Sample<f32>>,
    resistance_target: Option<Sample<u32>>,
}

/// One sensor value, with when it arrived and which radio carried it.
#[derive(Debug, Clone, Copy)]
struct Sample<T> {
    value: T,
    at: Instant,
    source: ReadingSource,
}

impl<T: Copy> Sample<T> {
    fn is_fresh(&self, now: Instant) -> bool {
        now.duration_since(self.at) < SENSOR_TIMEOUT
    }
}

/// Should an incoming reading replace what is already held for a field?
///
/// ANT+ always wins. A BLE reading is only taken when no ANT+ reading for that
/// field is still live, so a dual-band sensor is read over ANT+ while BLE quietly
/// takes over if ANT+ stops — and a sensor that only speaks BLE, such as the
/// cadence this trainer does not broadcast over ANT+, keeps working as before.
fn should_replace<T: Copy>(
    held: &Option<Sample<T>>,
    incoming: ReadingSource,
    now: Instant,
) -> bool {
    match held {
        Some(held) if held.source == ReadingSource::Ant && incoming != ReadingSource::Ant => {
            !held.is_fresh(now)
        }
        _ => true,
    }
}

impl ReadingsTracker {
    /// Record whichever fields `incoming` carries, stamped with `now`, subject to
    /// the ANT+-first preference in [`should_replace`].
    pub fn merge(&mut self, incoming: LiveReadings, now: Instant) {
        let source = incoming.source;
        if let Some(v) = incoming.power_watts {
            if should_replace(&self.power, source, now) {
                self.power = Some(Sample {
                    value: v,
                    at: now,
                    source,
                });
            }
        }
        if let Some(v) = incoming.heart_rate_bpm {
            if should_replace(&self.heart_rate, source, now) {
                self.heart_rate = Some(Sample {
                    value: v,
                    at: now,
                    source,
                });
            }
        }
        if let Some(v) = incoming.cadence_rpm {
            if should_replace(&self.cadence, source, now) {
                self.cadence = Some(Sample {
                    value: v,
                    at: now,
                    source,
                });
            }
        }
        if let Some(v) = incoming.speed_kmh {
            if should_replace(&self.speed, source, now) {
                self.speed = Some(Sample {
                    value: v,
                    at: now,
                    source,
                });
            }
        }
        if let Some(v) = incoming.resistance_target_watts {
            if should_replace(&self.resistance_target, source, now) {
                self.resistance_target = Some(Sample {
                    value: v,
                    at: now,
                    source,
                });
            }
        }
    }

    /// The readings still considered live at `now`. Fields whose sensor has gone
    /// quiet come back as `None`, so they are neither shown nor recorded.
    pub fn current(&self, now: Instant) -> LiveReadings {
        fn fresh<T: Copy>(field: Option<Sample<T>>, now: Instant) -> Option<T> {
            field.filter(|s| s.is_fresh(now)).map(|s| s.value)
        }
        LiveReadings {
            power_watts: fresh(self.power, now),
            heart_rate_bpm: fresh(self.heart_rate, now),
            cadence_rpm: fresh(self.cadence, now),
            speed_kmh: fresh(self.speed, now),
            resistance_target_watts: fresh(self.resistance_target, now),
            // The merged view is a blend of whatever is live; the per-field source
            // has already done its work in `merge`.
            source: ReadingSource::default(),
        }
    }

    /// Names of the sensors that have a value but have gone quiet — for logging a
    /// dropout once, rather than silently showing a dash.
    pub fn stale_sensors(&self, now: Instant) -> Vec<&'static str> {
        fn expired<T: Copy>(field: Option<Sample<T>>, now: Instant) -> bool {
            field.is_some_and(|s| !s.is_fresh(now))
        }
        let mut names = Vec::new();
        if expired(self.power, now) {
            names.push("power");
        }
        if expired(self.heart_rate, now) {
            names.push("heart rate");
        }
        if expired(self.cadence, now) {
            names.push("cadence");
        }
        if expired(self.speed, now) {
            names.push("speed");
        }
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Sensor staleness ────────────────────────────────────────────────────
    // Regression cover for a ride that recorded 94 bpm in all 5086 data points
    // because the strap sent one value and then went quiet.

    fn hr_only(bpm: u32) -> LiveReadings {
        LiveReadings {
            heart_rate_bpm: Some(bpm),
            ..Default::default()
        }
    }

    #[test]
    fn should_report_a_reading_that_just_arrived() {
        let now = Instant::now();
        let mut t = ReadingsTracker::default();
        t.merge(hr_only(140), now);
        assert_eq!(t.current(now).heart_rate_bpm, Some(140));
    }

    #[test]
    fn should_drop_a_reading_once_its_sensor_goes_quiet() {
        let start = Instant::now();
        let mut t = ReadingsTracker::default();
        t.merge(hr_only(94), start);

        let still_live = start + SENSOR_TIMEOUT - Duration::from_millis(1);
        assert_eq!(t.current(still_live).heart_rate_bpm, Some(94));

        let expired = start + SENSOR_TIMEOUT;
        assert_eq!(
            t.current(expired).heart_rate_bpm,
            None,
            "a silent strap must not keep reporting its last value"
        );
        assert_eq!(t.stale_sensors(expired), vec!["heart rate"]);
    }

    #[test]
    fn should_keep_live_fields_when_another_sensor_drops() {
        // The trainer keeps transmitting while the strap dies — power must survive.
        let start = Instant::now();
        let mut t = ReadingsTracker::default();
        t.merge(hr_only(94), start);

        let later = start + SENSOR_TIMEOUT + Duration::from_secs(5);
        t.merge(
            LiveReadings {
                power_watts: Some(210),
                ..Default::default()
            },
            later,
        );

        let live = t.current(later);
        assert_eq!(live.power_watts, Some(210));
        assert_eq!(live.heart_rate_bpm, None);
    }

    #[test]
    fn should_revive_a_sensor_that_starts_reporting_again() {
        let start = Instant::now();
        let mut t = ReadingsTracker::default();
        t.merge(hr_only(94), start);
        let gap = start + SENSOR_TIMEOUT + Duration::from_secs(30);
        assert_eq!(t.current(gap).heart_rate_bpm, None);

        t.merge(hr_only(132), gap);
        assert_eq!(t.current(gap).heart_rate_bpm, Some(132));
        assert!(t.stale_sensors(gap).is_empty());
    }

    // ── ANT+ preference ─────────────────────────────────────────────────────

    fn hr_from(bpm: u32, source: ReadingSource) -> LiveReadings {
        LiveReadings {
            heart_rate_bpm: Some(bpm),
            source,
            ..Default::default()
        }
    }

    #[test]
    fn should_prefer_ant_over_ble_for_the_same_sensor() {
        let now = Instant::now();
        let mut t = ReadingsTracker::default();
        t.merge(hr_from(150, ReadingSource::Ant), now);
        t.merge(hr_from(94, ReadingSource::Ble), now);
        assert_eq!(
            t.current(now).heart_rate_bpm,
            Some(150),
            "a live ANT+ reading must not be overwritten by BLE"
        );
    }

    #[test]
    fn should_use_ble_when_ant_offers_nothing() {
        // The trainer broadcasts no cadence over ANT+, so BLE has to serve it.
        let now = Instant::now();
        let mut t = ReadingsTracker::default();
        t.merge(
            LiveReadings {
                cadence_rpm: Some(88),
                source: ReadingSource::Ble,
                ..Default::default()
            },
            now,
        );
        assert_eq!(t.current(now).cadence_rpm, Some(88));
    }

    #[test]
    fn should_fall_back_to_ble_once_ant_goes_quiet() {
        let start = Instant::now();
        let mut t = ReadingsTracker::default();
        t.merge(hr_from(150, ReadingSource::Ant), start);

        // While ANT+ is still live, BLE is ignored.
        let during = start + Duration::from_secs(5);
        t.merge(hr_from(94, ReadingSource::Ble), during);
        assert_eq!(t.current(during).heart_rate_bpm, Some(150));

        // Once ANT+ has gone quiet, BLE takes over rather than leaving a gap.
        let after = start + SENSOR_TIMEOUT + Duration::from_secs(1);
        t.merge(hr_from(96, ReadingSource::Ble), after);
        assert_eq!(t.current(after).heart_rate_bpm, Some(96));
    }

    #[test]
    fn should_return_to_ant_when_it_comes_back() {
        let start = Instant::now();
        let mut t = ReadingsTracker::default();
        let after = start + SENSOR_TIMEOUT + Duration::from_secs(1);
        t.merge(hr_from(96, ReadingSource::Ble), after);
        assert_eq!(t.current(after).heart_rate_bpm, Some(96));

        t.merge(hr_from(151, ReadingSource::Ant), after);
        assert_eq!(t.current(after).heart_rate_bpm, Some(151));
    }

    #[test]
    fn should_report_nothing_before_any_sensor_reports() {
        let now = Instant::now();
        let t = ReadingsTracker::default();
        let live = t.current(now);
        assert_eq!(live.heart_rate_bpm, None);
        assert_eq!(live.power_watts, None);
        // Nothing has ever reported, so nothing counts as having dropped out.
        assert!(t.stale_sensors(now).is_empty());
    }

    use crate::data::workout::Segment;

    fn dp(elapsed: u32, power: Option<u32>) -> DataPoint {
        DataPoint {
            elapsed_secs: elapsed,
            power_watts: power,
            target_watts: None,
            heart_rate_bpm: None,
            cadence_rpm: None,
            speed_kmh: None,
            lat: None,
            lng: None,
            altitude_m: None,
        }
    }

    /// Session with constant power at every second from 0..count.
    fn session_const_power(watts: u32, count: u32) -> Session {
        let mut s = Session::new(None);
        for i in 0..count {
            s.data_points.push(dp(i, Some(watts)));
        }
        s
    }

    // ── average_power ──────────────────────────────────────────────────────────

    #[test]
    fn average_power_is_none_with_no_power_data() {
        let mut s = Session::new(None);
        s.data_points.push(dp(0, None));
        assert!(s.average_power().is_none());
    }

    #[test]
    fn average_power_ignores_missing_readings() {
        let mut s = Session::new(None);
        s.data_points.push(dp(0, Some(100)));
        s.data_points.push(dp(1, None));
        s.data_points.push(dp(2, Some(300)));
        // Only the two present readings count: (100 + 300) / 2 = 200
        assert_eq!(s.average_power(), Some(200.0));
    }

    // ── normalised_power ────────────────────────────────────────────────────────

    #[test]
    fn normalised_power_requires_30_seconds_of_data() {
        let s = session_const_power(200, 29);
        assert!(s.normalised_power().is_none());
    }

    #[test]
    fn normalised_power_equals_average_for_constant_power() {
        // With constant power the 30s rolling average is flat, so NP == average power.
        let s = session_const_power(200, 60);
        let np = s.normalised_power().expect("60s of data yields NP");
        assert!((np - 200.0).abs() < 0.01, "NP was {np}");
    }

    // ── peak_power_for_duration ──────────────────────────────────────────────────

    #[test]
    fn peak_power_is_none_when_fewer_points_than_window() {
        let s = session_const_power(200, 5);
        assert!(s.peak_power_for_duration(10).is_none());
    }

    #[test]
    fn peak_power_finds_highest_rolling_window() {
        let mut s = Session::new(None);
        let powers = [100, 100, 400, 400, 100, 100];
        for (i, p) in powers.iter().enumerate() {
            s.data_points.push(dp(i as u32, Some(*p)));
        }
        // Best 2-point window is the two 400s → 400.
        assert_eq!(s.peak_power_for_duration(2), Some(400));
    }

    // ── tss ──────────────────────────────────────────────────────────────────────

    #[test]
    fn tss_is_none_without_enough_data_for_np() {
        let s = session_const_power(200, 10);
        assert!(s.tss(250).is_none());
    }

    #[test]
    fn tss_is_100_for_one_hour_at_ftp() {
        // One hour at exactly FTP → IF = 1.0 → TSS = 100.
        let mut s = session_const_power(250, 3600);
        s.started_at = Utc::now() - chrono::Duration::seconds(3600);
        s.ended_at = Some(Utc::now());
        let tss = s.tss(250).expect("3600s of data yields TSS");
        assert!((tss - 100.0).abs() < 1.0, "TSS was {tss}");
    }

    // ── duration_secs / kilojoules ───────────────────────────────────────────────

    #[test]
    fn duration_uses_started_and_ended_timestamps() {
        let mut s = Session::new(None);
        s.started_at = Utc::now() - chrono::Duration::seconds(120);
        s.ended_at = Some(s.started_at + chrono::Duration::seconds(90));
        assert_eq!(s.duration_secs(), 90);
    }

    #[test]
    fn kilojoules_is_zero_without_power() {
        let mut s = Session::new(None);
        s.ended_at = Some(s.started_at + chrono::Duration::seconds(3600));
        assert_eq!(s.kilojoules(), 0.0);
    }

    // ── interval_analysis / compliance_pct ───────────────────────────────────────

    #[test]
    fn compliance_is_none_with_no_active_segments() {
        // A single recovery segment (≤55 % FTP) is not "active".
        let segments = vec![Segment::steady(60, 50.0, "Recovery")];
        let s = session_const_power(100, 60);
        assert!(s.compliance_pct(&segments, 200).is_none());
    }

    #[test]
    fn compliance_is_100_when_on_target_throughout() {
        // One 60s segment at 100 % FTP (200 W). Hold exactly 200 W the whole time.
        let segments = vec![Segment::steady(60, 100.0, "Effort")];
        let s = session_const_power(200, 60);
        assert_eq!(s.compliance_pct(&segments, 200), Some(100));
    }

    #[test]
    fn compliance_is_zero_when_far_off_target() {
        // Target 200 W ±10 % = [180, 220]; riding 100 W is outside the band.
        let segments = vec![Segment::steady(60, 100.0, "Effort")];
        let s = session_const_power(100, 60);
        assert_eq!(s.compliance_pct(&segments, 200), Some(0));
    }

    #[test]
    fn interval_analysis_computes_per_segment_average() {
        let segments = vec![
            Segment::steady(3, 100.0, "A"),
            Segment::steady(3, 50.0, "B"),
        ];
        let mut s = Session::new(None);
        // Segment A: seconds 0,1,2 at 200 W; Segment B: seconds 3,4,5 at 100 W.
        for i in 0..3 {
            s.data_points.push(dp(i, Some(200)));
        }
        for i in 3..6 {
            s.data_points.push(dp(i, Some(100)));
        }
        let stats = s.interval_analysis(&segments, 200);
        assert_eq!(stats.len(), 2);
        assert_eq!(stats[0].avg_watts, Some(200.0));
        assert_eq!(stats[0].target_watts, 200);
        assert!(stats[0].is_active);
        assert_eq!(stats[1].avg_watts, Some(100.0));
        assert!(!stats[1].is_active); // 50 % FTP segment is not active
    }
}
