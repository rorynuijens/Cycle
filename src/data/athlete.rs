use serde::{Deserialize, Serialize};

/// Athlete profile stored in SQLite and loaded at startup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AthleteProfile {
    pub id: i64,
    pub name: String,
    /// Body weight in kg — used for W/kg calculations
    pub weight_kg: f32,
    /// Functional Threshold Power in watts
    pub ftp_watts: u32,
    /// Maximum heart rate in bpm
    pub max_hr: u32,
    /// Resting heart rate in bpm
    pub resting_hr: u32,
}

impl Default for AthleteProfile {
    fn default() -> Self {
        Self {
            id: 0,
            name: String::from("Athlete"),
            weight_kg: 70.0,
            ftp_watts: 200,
            max_hr: 185,
            resting_hr: 55,
        }
    }
}

/// RGB colour tuples for Coggan power zones Z1–Z7, for use in Cairo drawing code.
/// Indices map directly to the zone number minus one (Z1 = index 0, Z7 = index 6).
pub const ZONE_COLORS: [(f64, f64, f64); 7] = [
    (0.34, 0.89, 0.53), // Z1 Recovery
    (0.47, 0.68, 0.93), // Z2 Endurance
    (0.97, 0.89, 0.36), // Z3 Tempo
    (1.00, 0.64, 0.28), // Z4 Threshold
    (1.00, 0.48, 0.39), // Z5 VO₂Max
    (0.84, 0.20, 0.20), // Z6 Anaerobic
    (0.60, 0.10, 0.60), // Z7 Neuromuscular
];

/// Return the 0-based Coggan power zone index (0 = Z1 … 6 = Z7) for a wattage.
/// Returns 0 when `ftp` is zero to avoid division by zero.
pub fn power_zone_index(watts: u32, ftp: u32) -> usize {
    if ftp == 0 {
        return 0;
    }
    let pct = (watts as f64 / ftp as f64) * 100.0;
    match pct as u32 {
        0..=55 => 0,
        56..=75 => 1,
        76..=90 => 2,
        91..=105 => 3,
        106..=120 => 4,
        121..=150 => 5,
        _ => 6,
    }
}

impl AthleteProfile {
    /// Watts per kilogram at FTP
    #[allow(dead_code)]
    pub fn watts_per_kg(&self) -> f32 {
        self.ftp_watts as f32 / self.weight_kg
    }

    /// Return the Coggan power zone (1–7) for a given instantaneous wattage.
    pub fn power_zone(&self, watts: u32) -> PowerZone {
        let pct = (watts as f32 / self.ftp_watts as f32) * 100.0;
        match pct as u32 {
            0..=55 => PowerZone::ActiveRecovery,
            56..=75 => PowerZone::Endurance,
            76..=90 => PowerZone::Tempo,
            91..=105 => PowerZone::Threshold,
            106..=120 => PowerZone::Vo2Max,
            121..=150 => PowerZone::Anaerobic,
            _ => PowerZone::Neuromuscular,
        }
    }

    /// Return the heart-rate zone (1–5) for a given HR reading.
    #[allow(dead_code)]
    pub fn hr_zone(&self, bpm: u32) -> HrZone {
        let hrr = self.max_hr - self.resting_hr;
        let pct = ((bpm.saturating_sub(self.resting_hr)) as f32 / hrr as f32) * 100.0;
        match pct as u32 {
            0..=59 => HrZone::Recovery,
            60..=69 => HrZone::Aerobic,
            70..=79 => HrZone::Tempo,
            80..=89 => HrZone::Threshold,
            _ => HrZone::Anaerobic,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerZone {
    ActiveRecovery,
    Endurance,
    Tempo,
    Threshold,
    Vo2Max,
    Anaerobic,
    Neuromuscular,
}

impl PowerZone {
    #[allow(dead_code)]
    pub fn label(&self) -> &'static str {
        match self {
            Self::ActiveRecovery => "Z1 Recovery",
            Self::Endurance => "Z2 Endurance",
            Self::Tempo => "Z3 Tempo",
            Self::Threshold => "Z4 Threshold",
            Self::Vo2Max => "Z5 VO₂Max",
            Self::Anaerobic => "Z6 Anaerobic",
            Self::Neuromuscular => "Z7 Neuromuscular",
        }
    }

    /// RGB tuple for Cairo drawing — all values in 0.0–1.0 range.
    pub fn rgb(&self) -> (f64, f64, f64) {
        match self {
            Self::ActiveRecovery => (0.34, 0.89, 0.53),
            Self::Endurance => (0.47, 0.68, 0.93),
            Self::Tempo => (0.97, 0.89, 0.36),
            Self::Threshold => (1.00, 0.64, 0.28),
            Self::Vo2Max => (1.00, 0.48, 0.39),
            Self::Anaerobic => (0.84, 0.20, 0.20),
            Self::Neuromuscular => (0.60, 0.10, 0.60),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum HrZone {
    Recovery,
    Aerobic,
    Tempo,
    Threshold,
    Anaerobic,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn athlete_ftp(ftp: u32) -> AthleteProfile {
        AthleteProfile {
            ftp_watts: ftp,
            ..AthleteProfile::default()
        }
    }

    // ── power_zone (PowerZone enum) ────────────────────────────────────────────

    #[test]
    fn should_return_z1_recovery_when_power_is_zero() {
        assert_eq!(athlete_ftp(200).power_zone(0), PowerZone::ActiveRecovery);
    }

    #[test]
    fn should_return_z1_recovery_at_55_percent_ftp() {
        // 110 W = 55 % of 200 W FTP — top of Z1
        assert_eq!(athlete_ftp(200).power_zone(110), PowerZone::ActiveRecovery);
    }

    #[test]
    fn should_return_z2_endurance_at_65_percent_ftp() {
        assert_eq!(athlete_ftp(200).power_zone(130), PowerZone::Endurance);
    }

    #[test]
    fn should_return_z3_tempo_at_90_percent_ftp() {
        // 180 W = 90 % of 200 W — top of Z3
        assert_eq!(athlete_ftp(200).power_zone(180), PowerZone::Tempo);
    }

    #[test]
    fn should_return_z4_threshold_at_91_percent_ftp() {
        // 182 W = 91 % of 200 W — bottom of Z4 (boundary from CLAUDE.md §3.2)
        assert_eq!(athlete_ftp(200).power_zone(182), PowerZone::Threshold);
    }

    #[test]
    fn should_return_z4_threshold_when_power_is_exactly_ftp() {
        assert_eq!(athlete_ftp(250).power_zone(250), PowerZone::Threshold);
    }

    #[test]
    fn should_return_z5_vo2max_at_112_percent_ftp() {
        assert_eq!(athlete_ftp(200).power_zone(225), PowerZone::Vo2Max);
    }

    #[test]
    fn should_return_z6_anaerobic_at_135_percent_ftp() {
        assert_eq!(athlete_ftp(200).power_zone(270), PowerZone::Anaerobic);
    }

    #[test]
    fn should_return_z7_neuromuscular_above_150_percent_ftp() {
        assert_eq!(athlete_ftp(200).power_zone(350), PowerZone::Neuromuscular);
    }

    // ── power_zone_index (0-based, f64 maths) ───────────────────────────────────

    #[test]
    fn should_return_index_zero_when_ftp_is_zero() {
        // Guards against division by zero.
        assert_eq!(power_zone_index(250, 0), 0);
    }

    #[test]
    fn should_map_zone_index_boundaries() {
        let ftp = 200;
        assert_eq!(power_zone_index(0, ftp), 0); // Z1
        assert_eq!(power_zone_index(110, ftp), 0); // 55 % — top of Z1
        assert_eq!(power_zone_index(112, ftp), 1); // 56 % — bottom of Z2
        assert_eq!(power_zone_index(150, ftp), 1); // 75 % — top of Z2
        assert_eq!(power_zone_index(152, ftp), 2); // 76 % — bottom of Z3
        assert_eq!(power_zone_index(182, ftp), 3); // 91 % — bottom of Z4
        assert_eq!(power_zone_index(210, ftp), 3); // 105 % — top of Z4
        assert_eq!(power_zone_index(212, ftp), 4); // 106 % — bottom of Z5
        assert_eq!(power_zone_index(242, ftp), 5); // 121 % — bottom of Z6
        assert_eq!(power_zone_index(302, ftp), 6); // 151 % — Z7
    }

    #[test]
    fn zone_index_maps_into_zone_colors() {
        // Every index returned must be a valid index into ZONE_COLORS.
        for watts in [0, 110, 200, 300, 600] {
            assert!(power_zone_index(watts, 200) < ZONE_COLORS.len());
        }
    }

    // ── hr_zone ─────────────────────────────────────────────────────────────────

    #[test]
    fn should_map_hr_zones_by_reserve() {
        // max 185, resting 55 → HR reserve = 130 bpm
        let a = AthleteProfile::default();
        assert_eq!(a.hr_zone(55), HrZone::Recovery); // 0 % reserve
        assert_eq!(a.hr_zone(120), HrZone::Recovery); // 50 % reserve
        assert_eq!(a.hr_zone(140), HrZone::Aerobic); // ~65 %
        assert_eq!(a.hr_zone(150), HrZone::Tempo); // ~73 %
        assert_eq!(a.hr_zone(165), HrZone::Threshold); // ~85 %
        assert_eq!(a.hr_zone(180), HrZone::Anaerobic); // ~96 %
    }

    #[test]
    fn hr_below_resting_is_recovery() {
        // saturating_sub prevents underflow for readings below resting HR.
        assert_eq!(AthleteProfile::default().hr_zone(40), HrZone::Recovery);
    }

    // ── watts_per_kg ──────────────────────────────────────────────────────────────

    #[test]
    fn should_compute_watts_per_kg() {
        let a = AthleteProfile {
            ftp_watts: 280,
            weight_kg: 70.0,
            ..AthleteProfile::default()
        };
        assert!((a.watts_per_kg() - 4.0).abs() < f32::EPSILON);
    }
}
