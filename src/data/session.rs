use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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
}

#[cfg(test)]
mod tests {
    use super::*;
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
