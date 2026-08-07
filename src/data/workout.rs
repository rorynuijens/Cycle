use serde::{Deserialize, Serialize};

/// A complete structured workout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workout {
    pub id: i64,
    pub name: String,
    pub description: String,
    /// Total duration in seconds
    pub duration_secs: u32,
    /// Training Stress Score
    pub tss: f32,
    pub category: WorkoutCategory,
    pub segments: Vec<Segment>,
}

impl Workout {
    /// Build a workout from its segments, deriving duration and TSS.
    ///
    /// TSS is summed per segment from the intensity factor at the segment's
    /// mid-point: `hours × IF² × 100`. A ramp is scored at its average, which
    /// slightly understates it — IF² is convex — but matches how the rest of
    /// the app reports planned load.
    pub fn from_segments(
        name: &str,
        description: &str,
        category: WorkoutCategory,
        segments: Vec<Segment>,
    ) -> Self {
        let duration_secs = segments.iter().map(|s| s.duration_secs).sum();
        let tss = segments.iter().map(Segment::tss_contribution).sum();
        Self {
            id: 0,
            name: name.to_string(),
            description: description.to_string(),
            duration_secs,
            tss,
            category,
            segments,
        }
    }

    /// Weighted average % of FTP across all segments.
    #[allow(dead_code)]
    pub fn average_ftp_percent(&self) -> f32 {
        let total_secs: u32 = self.segments.iter().map(|s| s.duration_secs).sum();
        if total_secs == 0 {
            return 0.0;
        }
        let weighted: f32 = self
            .segments
            .iter()
            .map(|s| {
                let mid = (s.power_low_pct + s.power_high_pct) / 2.0;
                mid * s.duration_secs as f32
            })
            .sum();
        weighted / total_secs as f32
    }

    /// Returns a sample workout for UI previews and tests.
    pub fn sample_threshold() -> Self {
        Self {
            id: 1,
            name: "Cruise Intervals".into(),
            description: "Classic threshold cruise intervals.".into(),
            duration_secs: 3600,
            tss: 72.0,
            category: WorkoutCategory::Threshold,
            segments: vec![
                Segment::steady(600, 50.0, "Warm-up"),
                Segment::steady(480, 98.0, "Interval 1"),
                Segment::steady(300, 50.0, "Recovery"),
                Segment::steady(480, 98.0, "Interval 2"),
                Segment::steady(300, 50.0, "Recovery"),
                Segment::steady(480, 98.0, "Interval 3"),
                Segment::steady(300, 50.0, "Recovery"),
                Segment::steady(480, 98.0, "Interval 4"),
                Segment::steady(300, 50.0, "Recovery"),
                Segment::steady(480, 98.0, "Interval 5"),
                Segment::steady(600, 40.0, "Cool-down"),
            ],
        }
    }
}

/// A single segment within a workout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    pub duration_secs: u32,
    /// Power low end — % of FTP. Equal to high for steady segments.
    pub power_low_pct: f32,
    /// Power high end — % of FTP. Equal to low for steady segments.
    pub power_high_pct: f32,
    pub label: Option<String>,
    pub cadence_target: Option<u32>,
}

impl Segment {
    pub fn steady(duration_secs: u32, ftp_pct: f32, label: &str) -> Self {
        Self {
            duration_secs,
            power_low_pct: ftp_pct,
            power_high_pct: ftp_pct,
            label: Some(label.into()),
            cadence_target: None,
        }
    }

    pub fn ramp(duration_secs: u32, from_pct: f32, to_pct: f32, label: &str) -> Self {
        Self {
            duration_secs,
            power_low_pct: from_pct,
            power_high_pct: to_pct,
            label: Some(label.into()),
            cadence_target: None,
        }
    }

    /// This segment's share of the workout's Training Stress Score.
    ///
    /// `hours × IF² × 100`, with the intensity factor taken at the segment's
    /// mid-point power.
    pub fn tss_contribution(&self) -> f32 {
        let mid_pct = (self.power_low_pct + self.power_high_pct) / 2.0;
        let intensity = mid_pct / 100.0;
        (self.duration_secs as f32 / 3600.0) * intensity * intensity * 100.0
    }

    pub fn is_ramp(&self) -> bool {
        (self.power_high_pct - self.power_low_pct).abs() > 0.5
    }

    /// Target power at a given elapsed time within the segment.
    pub fn target_power_at(&self, elapsed_secs: u32, ftp: u32) -> u32 {
        if self.is_ramp() {
            let progress = elapsed_secs as f32 / self.duration_secs as f32;
            let pct = self.power_low_pct + (self.power_high_pct - self.power_low_pct) * progress;
            (pct / 100.0 * ftp as f32) as u32
        } else {
            (self.power_low_pct / 100.0 * ftp as f32) as u32
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WorkoutCategory {
    Recovery,
    Endurance,
    Tempo,
    SweetSpot,
    Threshold,
    Vo2Max,
    Anaerobic,
    Custom,
}

impl WorkoutCategory {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Recovery => "Recovery",
            Self::Endurance => "Endurance",
            Self::Tempo => "Tempo",
            Self::SweetSpot => "Sweet Spot",
            Self::Threshold => "Threshold",
            Self::Vo2Max => "VO₂Max",
            Self::Anaerobic => "Anaerobic",
            Self::Custom => "Custom",
        }
    }

    pub fn as_db_str(&self) -> &'static str {
        match self {
            Self::Recovery => "Recovery",
            Self::Endurance => "Endurance",
            Self::Tempo => "Tempo",
            Self::SweetSpot => "SweetSpot",
            Self::Threshold => "Threshold",
            Self::Vo2Max => "Vo2Max",
            Self::Anaerobic => "Anaerobic",
            Self::Custom => "Custom",
        }
    }

    pub fn from_db_str(s: &str) -> Self {
        match s {
            "Recovery" => Self::Recovery,
            "Endurance" => Self::Endurance,
            "Tempo" => Self::Tempo,
            "SweetSpot" => Self::SweetSpot,
            "Threshold" => Self::Threshold,
            "Vo2Max" => Self::Vo2Max,
            "Anaerobic" => Self::Anaerobic,
            _ => Self::Custom,
        }
    }
}

impl Workout {
    /// Returns the full structured training library of 100 workouts.
    pub fn workout_library() -> Vec<Workout> {
        use WorkoutCategory::*;
        vec![
            // ── Recovery (8) ───────────────────────────────────────────────────
            mk(
                "Active Recovery 30",
                "Light spin to flush the legs after a hard effort.",
                Recovery,
                vec![wu(5), st(20 * 60, 45.0, "Easy Spin"), cd(5)],
            ),
            mk(
                "Active Recovery 45",
                "Extended easy ride to promote circulation.",
                Recovery,
                vec![wu(5), st(35 * 60, 45.0, "Easy Spin"), cd(5)],
            ),
            mk(
                "Active Recovery 60",
                "One-hour easy ride for active recovery days.",
                Recovery,
                vec![wu(10), st(40 * 60, 45.0, "Easy Spin"), cd(10)],
            ),
            mk(
                "Easy Spin",
                "Comfortable ride at low intensity to keep the legs moving.",
                Recovery,
                vec![wu(5), st(35 * 60, 50.0, "Easy Spin"), cd(5)],
            ),
            mk(
                "Recovery Ride",
                "Pure recovery — keep power low throughout.",
                Recovery,
                vec![wu(10), st(40 * 60, 50.0, "Recovery"), cd(10)],
            ),
            mk(
                "Gentle Pedaling",
                "75 minutes at the bottom of Zone 1.",
                Recovery,
                vec![wu(10), st(55 * 60, 48.0, "Gentle"), cd(10)],
            ),
            mk(
                "Low Cadence Recovery",
                "Slow-cadence easy spinning for muscular recovery.",
                Recovery,
                vec![wu(5), st(35 * 60, 42.0, "Low Cadence"), cd(5)],
            ),
            mk(
                "Post-Race Flush",
                "Gentle ramp to help clear lactate after a race or hard event.",
                Recovery,
                vec![wu(5), Segment::ramp(20 * 60, 40.0, 50.0, "Flush"), cd(5)],
            ),
            // ── Endurance (22) ─────────────────────────────────────────────────
            mk(
                "Endurance 60",
                "Bread-and-butter Zone 2 aerobic ride.",
                Endurance,
                vec![wu(10), st(40 * 60, 65.0, "Endurance"), cd(10)],
            ),
            mk(
                "Endurance 75",
                "75-minute Zone 2 steady state.",
                Endurance,
                vec![wu(10), st(55 * 60, 65.0, "Endurance"), cd(10)],
            ),
            mk(
                "Endurance 90",
                "90-minute steady aerobic ride.",
                Endurance,
                vec![wu(10), st(70 * 60, 65.0, "Endurance"), cd(10)],
            ),
            mk(
                "Endurance 105",
                "Extended Zone 2 block for aerobic development.",
                Endurance,
                vec![wu(10), st(85 * 60, 65.0, "Endurance"), cd(10)],
            ),
            mk(
                "Endurance 120",
                "Two-hour aerobic foundation ride.",
                Endurance,
                vec![wu(10), st(100 * 60, 65.0, "Endurance"), cd(10)],
            ),
            mk(
                "Zone 2 Foundation",
                "90 minutes at the lower end of Zone 2.",
                Endurance,
                vec![wu(10), st(70 * 60, 63.0, "Zone 2"), cd(10)],
            ),
            mk(
                "Aerobic Base",
                "75-minute aerobic conditioning block.",
                Endurance,
                vec![wu(10), st(55 * 60, 67.0, "Aerobic Base"), cd(10)],
            ),
            mk(
                "Steady Miles",
                "Continuous effort at the top of Zone 2.",
                Endurance,
                vec![wu(10), st(40 * 60, 70.0, "Steady"), cd(10)],
            ),
            mk(
                "Long Ride Prep",
                "Builds endurance for long outdoor rides.",
                Endurance,
                vec![wu(10), st(70 * 60, 68.0, "Long Ride"), cd(10)],
            ),
            mk(
                "Aerobic Intervals",
                "Two 20-minute aerobic blocks with a recovery valley.",
                Endurance,
                chain(&[
                    vec![wu(10)],
                    ivls(2, 20 * 60, 70.0, 5 * 60, "Aerobic"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "Big Ring Light",
                "Simulate outdoor big-ring riding at moderate intensity.",
                Endurance,
                vec![wu(10), st(40 * 60, 72.0, "Big Ring"), cd(10)],
            ),
            mk(
                "Endurance With Openers",
                "Endurance base with short punchy efforts to wake up the legs.",
                Endurance,
                chain(&[
                    vec![wu(10), st(30 * 60, 65.0, "Endurance")],
                    ivls(3, 3 * 60, 80.0, 2 * 60, "Opener"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "Fasted Simulation",
                "90-minute effort at low-to-moderate power to train fat oxidation.",
                Endurance,
                vec![wu(10), st(70 * 60, 63.0, "Fasted Ride"), cd(10)],
            ),
            mk(
                "Aerobic Foundation",
                "Core aerobic fitness builder at Zone 2.",
                Endurance,
                vec![wu(10), st(55 * 60, 65.0, "Foundation"), cd(10)],
            ),
            mk(
                "Aerobic Power",
                "Sustained effort at the upper end of Zone 2.",
                Endurance,
                vec![wu(10), st(55 * 60, 70.0, "Aerobic Power"), cd(10)],
            ),
            mk(
                "Z2 Progression",
                "Progressive Zone 2 ride ramping from 62 to 68% FTP.",
                Endurance,
                vec![
                    wu(10),
                    Segment::ramp(70 * 60, 62.0, 68.0, "Z2 Progression"),
                    cd(10),
                ],
            ),
            mk(
                "Steady Aerobic",
                "Classic Zone 2 building block.",
                Endurance,
                vec![wu(10), st(40 * 60, 65.0, "Steady"), cd(10)],
            ),
            mk(
                "Long Day Easy",
                "Two-hour easy base at the bottom of Zone 2.",
                Endurance,
                vec![wu(10), st(100 * 60, 63.0, "Long Day"), cd(10)],
            ),
            mk(
                "Low Stress Riding",
                "Relaxed aerobic work without taxing recovery.",
                Endurance,
                vec![wu(10), st(55 * 60, 67.0, "Low Stress"), cd(10)],
            ),
            mk(
                "Easy Recovery Build",
                "Gently ramps from recovery into endurance pace.",
                Endurance,
                vec![wu(10), Segment::ramp(40 * 60, 60.0, 68.0, "Build"), cd(10)],
            ),
            mk(
                "Foundation Ride",
                "90-minute core aerobic session.",
                Endurance,
                vec![wu(10), st(70 * 60, 65.0, "Foundation"), cd(10)],
            ),
            mk(
                "Aerobic Capacity",
                "Extended aerobic base — 105 minutes of steady Zone 2.",
                Endurance,
                vec![wu(10), st(85 * 60, 65.0, "Aerobic Cap"), cd(10)],
            ),
            // ── Tempo (15) ─────────────────────────────────────────────────────
            mk(
                "Tempo 20",
                "Single 20-minute tempo block to develop lactate threshold.",
                Tempo,
                vec![wu(10), st(20 * 60, 83.0, "Tempo"), rv(5), cd(10)],
            ),
            mk(
                "Tempo 30",
                "Sustained 30-minute effort in the tempo zone.",
                Tempo,
                vec![wu(10), st(30 * 60, 83.0, "Tempo"), rv(5), cd(5)],
            ),
            mk(
                "2x15 Tempo",
                "Two 15-minute tempo intervals.",
                Tempo,
                chain(&[
                    vec![wu(10)],
                    ivls(2, 15 * 60, 83.0, 5 * 60, "Tempo"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "3x10 Tempo",
                "Three 10-minute tempo blocks with recovery.",
                Tempo,
                chain(&[
                    vec![wu(10)],
                    ivls(3, 10 * 60, 83.0, 4 * 60, "Tempo"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "Tempo Threshold",
                "Two hard 20-minute tempo intervals at 85% FTP.",
                Tempo,
                chain(&[
                    vec![wu(10)],
                    ivls(2, 20 * 60, 85.0, 5 * 60, "Tempo"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "Endurance-Tempo Mix",
                "Three 15-minute tempo efforts on an endurance base.",
                Tempo,
                chain(&[
                    vec![wu(10)],
                    ivls(3, 15 * 60, 83.0, 5 * 60, "Tempo"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "Ascending Tempo",
                "Progressive tempo blocks stepping from 80 to 88% FTP.",
                Tempo,
                vec![
                    wu(10),
                    st(12 * 60, 80.0, "Tempo 1"),
                    rv(3),
                    st(12 * 60, 84.0, "Tempo 2"),
                    rv(3),
                    st(12 * 60, 88.0, "Tempo 3"),
                    cd(10),
                ],
            ),
            mk(
                "Tempo Over-Unders",
                "Alternating under/over tempo to build lactate tolerance.",
                Tempo,
                vec![
                    wu(15),
                    st(5 * 60, 80.0, "Under 1"),
                    st(3 * 60, 88.0, "Over 1"),
                    rv(3),
                    st(5 * 60, 80.0, "Under 2"),
                    st(3 * 60, 88.0, "Over 2"),
                    rv(3),
                    st(5 * 60, 80.0, "Under 3"),
                    st(3 * 60, 88.0, "Over 3"),
                    cd(12),
                ],
            ),
            mk(
                "Long Tempo",
                "Single 45-minute sustained tempo effort.",
                Tempo,
                vec![wu(10), st(45 * 60, 80.0, "Long Tempo"), cd(10)],
            ),
            mk(
                "Tempo Base",
                "30-minute moderate tempo block for building pace.",
                Tempo,
                vec![wu(10), st(30 * 60, 78.0, "Tempo Base"), rv(5), cd(10)],
            ),
            mk(
                "Short Tempo Intervals",
                "Four 8-minute tempo efforts with short recoveries.",
                Tempo,
                chain(&[
                    vec![wu(10)],
                    ivls(4, 8 * 60, 85.0, 3 * 60, "Tempo"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "Cadence Tempo",
                "Two 20-minute tempo blocks — focus on high, smooth cadence.",
                Tempo,
                chain(&[
                    vec![wu(10)],
                    ivls(2, 20 * 60, 82.0, 5 * 60, "Cadence Tempo"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "Tempo Progressions",
                "Three 15-minute tempo blocks stepping up in intensity.",
                Tempo,
                vec![
                    wu(10),
                    st(15 * 60, 80.0, "Tempo 1"),
                    rv(3),
                    st(15 * 60, 84.0, "Tempo 2"),
                    rv(3),
                    st(15 * 60, 88.0, "Tempo 3"),
                    cd(10),
                ],
            ),
            mk(
                "Fatigue Tempo",
                "Two long 25-minute tempo blocks to build durability.",
                Tempo,
                chain(&[
                    vec![wu(10)],
                    ivls(2, 25 * 60, 82.0, 5 * 60, "Tempo"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "Big Ring Tempo",
                "Two 30-minute tempo blocks — simulates a hilly outdoor ride.",
                Tempo,
                chain(&[
                    vec![wu(10)],
                    ivls(2, 30 * 60, 83.0, 5 * 60, "Tempo"),
                    vec![cd(10)],
                ]),
            ),
            // ── Sweet Spot (12) ────────────────────────────────────────────────
            mk(
                "Sweet Spot 2x15",
                "Two 15-minute efforts just below threshold.",
                SweetSpot,
                chain(&[
                    vec![wu(10)],
                    ivls(2, 15 * 60, 90.0, 5 * 60, "Sweet Spot"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "Sweet Spot 2x20",
                "Two 20-minute sweet spot efforts.",
                SweetSpot,
                chain(&[
                    vec![wu(10)],
                    ivls(2, 20 * 60, 90.0, 5 * 60, "Sweet Spot"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "Sweet Spot 3x12",
                "Three 12-minute sweet spot blocks.",
                SweetSpot,
                chain(&[
                    vec![wu(10)],
                    ivls(3, 12 * 60, 90.0, 4 * 60, "Sweet Spot"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "Sweet Spot 3x15",
                "Three 15-minute efforts at 90% FTP.",
                SweetSpot,
                chain(&[
                    vec![wu(10)],
                    ivls(3, 15 * 60, 90.0, 5 * 60, "Sweet Spot"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "Sweet Spot 4x12",
                "Four 12-minute sweet spot intervals.",
                SweetSpot,
                chain(&[
                    vec![wu(10)],
                    ivls(4, 12 * 60, 90.0, 3 * 60, "Sweet Spot"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "Sweet Spot Hard",
                "Two 20-minute efforts at the top of sweet spot.",
                SweetSpot,
                chain(&[
                    vec![wu(10)],
                    ivls(2, 20 * 60, 93.0, 5 * 60, "Sweet Spot"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "Sweet Spot Base I",
                "Single 30-minute sweet spot block.",
                SweetSpot,
                vec![wu(10), st(30 * 60, 90.0, "Sweet Spot"), rv(5), cd(10)],
            ),
            mk(
                "Sweet Spot Base II",
                "Extended 45-minute sweet spot effort.",
                SweetSpot,
                vec![wu(10), st(45 * 60, 88.0, "Sweet Spot"), rv(5), cd(10)],
            ),
            mk(
                "Sweet Spot Progression",
                "Three ascending sweet spot blocks from 88 to 92% FTP.",
                SweetSpot,
                vec![
                    wu(10),
                    st(12 * 60, 88.0, "SS 1"),
                    rv(3),
                    st(12 * 60, 90.0, "SS 2"),
                    rv(3),
                    st(12 * 60, 92.0, "SS 3"),
                    cd(10),
                ],
            ),
            mk(
                "Sweet Spot Blocks",
                "Two long 30-minute sweet spot efforts.",
                SweetSpot,
                chain(&[
                    vec![wu(10)],
                    ivls(2, 30 * 60, 90.0, 5 * 60, "Sweet Spot"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "Extended Sweet Spot",
                "40-minute sustained effort at 90% FTP.",
                SweetSpot,
                vec![wu(10), st(40 * 60, 90.0, "Sweet Spot"), rv(5), cd(10)],
            ),
            mk(
                "Sweet Spot Opener",
                "Two short sweet spot blocks — great as a race-day opener.",
                SweetSpot,
                chain(&[
                    vec![wu(10)],
                    ivls(2, 15 * 60, 91.0, 5 * 60, "Sweet Spot"),
                    vec![cd(10)],
                ]),
            ),
            // ── Threshold (18) ─────────────────────────────────────────────────
            mk(
                "Threshold 2x10",
                "Two 10-minute efforts at 100% FTP.",
                Threshold,
                chain(&[
                    vec![wu(15)],
                    ivls(2, 10 * 60, 100.0, 5 * 60, "Threshold"),
                    vec![cd(15)],
                ]),
            ),
            mk(
                "Threshold 2x15",
                "Two 15-minute threshold intervals.",
                Threshold,
                chain(&[
                    vec![wu(10)],
                    ivls(2, 15 * 60, 100.0, 5 * 60, "Threshold"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "Threshold 3x10",
                "Three 10-minute efforts at FTP.",
                Threshold,
                chain(&[
                    vec![wu(10)],
                    ivls(3, 10 * 60, 100.0, 5 * 60, "Threshold"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "Threshold 4x8",
                "Four 8-minute threshold blocks.",
                Threshold,
                chain(&[
                    vec![wu(10)],
                    ivls(4, 8 * 60, 100.0, 4 * 60, "Threshold"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "FTP Ramp",
                "Progressive ramp from 90 to 105% FTP — builds threshold awareness.",
                Threshold,
                vec![
                    wu(15),
                    Segment::ramp(30 * 60, 90.0, 105.0, "FTP Ramp"),
                    cd(15),
                ],
            ),
            mk(
                "Cruise Intervals 2x20",
                "Classic cruise intervals at 98% FTP.",
                Threshold,
                chain(&[
                    vec![wu(10)],
                    ivls(2, 20 * 60, 98.0, 5 * 60, "Threshold"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "Cruise Intervals 3x15",
                "Three 15-minute cruise intervals.",
                Threshold,
                chain(&[
                    vec![wu(10)],
                    ivls(3, 15 * 60, 98.0, 5 * 60, "Threshold"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "Threshold Sustained",
                "Continuous 30-minute FTP effort.",
                Threshold,
                vec![wu(15), st(30 * 60, 98.0, "FTP Effort"), cd(15)],
            ),
            mk(
                "Over-Under 1",
                "Alternating under/over FTP segments to push lactate clearance.",
                Threshold,
                vec![
                    wu(15),
                    st(5 * 60, 96.0, "Under 1"),
                    st(3 * 60, 105.0, "Over 1"),
                    rv(4),
                    st(5 * 60, 96.0, "Under 2"),
                    st(3 * 60, 105.0, "Over 2"),
                    rv(4),
                    st(5 * 60, 96.0, "Under 3"),
                    st(3 * 60, 105.0, "Over 3"),
                    cd(10),
                ],
            ),
            mk(
                "Over-Under 2",
                "Extended over-under protocol with four sets.",
                Threshold,
                vec![
                    wu(10),
                    st(5 * 60, 96.0, "Under 1"),
                    st(3 * 60, 105.0, "Over 1"),
                    rv(3),
                    st(5 * 60, 96.0, "Under 2"),
                    st(3 * 60, 105.0, "Over 2"),
                    rv(3),
                    st(5 * 60, 96.0, "Under 3"),
                    st(3 * 60, 105.0, "Over 3"),
                    rv(3),
                    st(5 * 60, 96.0, "Under 4"),
                    st(3 * 60, 105.0, "Over 4"),
                    cd(10),
                ],
            ),
            mk(
                "Lactate Threshold",
                "Two 20-minute efforts at 98% FTP to lift your lactate ceiling.",
                Threshold,
                chain(&[
                    vec![wu(10)],
                    ivls(2, 20 * 60, 98.0, 5 * 60, "LT Effort"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "FTP Booster",
                "Three 12-minute intervals at 100% FTP.",
                Threshold,
                chain(&[
                    vec![wu(10)],
                    ivls(3, 12 * 60, 100.0, 4 * 60, "Threshold"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "Threshold Efforts",
                "Three 10-minute FTP efforts.",
                Threshold,
                chain(&[
                    vec![wu(10)],
                    ivls(3, 10 * 60, 100.0, 5 * 60, "FTP"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "40-Minute FTP",
                "Benchmark 40-minute effort at 98% FTP.",
                Threshold,
                vec![wu(10), st(40 * 60, 98.0, "FTP Effort"), cd(10)],
            ),
            mk(
                "Threshold Blocks",
                "Two long 25-minute threshold efforts.",
                Threshold,
                chain(&[
                    vec![wu(10)],
                    ivls(2, 25 * 60, 97.0, 5 * 60, "Threshold"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "Hard Tempo",
                "Two 18-minute efforts at 97% FTP.",
                Threshold,
                chain(&[
                    vec![wu(10)],
                    ivls(2, 18 * 60, 97.0, 5 * 60, "Hard Tempo"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "Functional Power",
                "Two 20-minute FTP development intervals.",
                Threshold,
                chain(&[
                    vec![wu(10)],
                    ivls(2, 20 * 60, 98.0, 5 * 60, "Functional"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "Threshold Builder",
                "Two demanding 25-minute blocks at 96% FTP.",
                Threshold,
                chain(&[
                    vec![wu(10)],
                    ivls(2, 25 * 60, 96.0, 5 * 60, "Threshold"),
                    vec![cd(15)],
                ]),
            ),
            // ── VO₂Max (15) ────────────────────────────────────────────────────
            mk(
                "4x4 Intervals",
                "Norwegian-style VO₂max intervals.",
                Vo2Max,
                chain(&[
                    vec![wu(10)],
                    ivls(4, 4 * 60, 115.0, 4 * 60, "VO₂Max"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "5x3 Intervals",
                "Five 3-minute VO₂max efforts.",
                Vo2Max,
                chain(&[
                    vec![wu(10)],
                    ivls(5, 3 * 60, 115.0, 3 * 60, "VO₂Max"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "8x2 Intervals",
                "Eight 2-minute maximal efforts.",
                Vo2Max,
                chain(&[
                    vec![wu(10)],
                    ivls(8, 2 * 60, 120.0, 2 * 60, "VO₂Max"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "6x3 Intervals",
                "Six 3-minute VO₂max intervals with equal rest.",
                Vo2Max,
                chain(&[
                    vec![wu(10)],
                    ivls(6, 3 * 60, 115.0, 3 * 60, "VO₂Max"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "4x5 Intervals",
                "Four 5-minute sustained VO₂max efforts.",
                Vo2Max,
                chain(&[
                    vec![wu(10)],
                    ivls(4, 5 * 60, 110.0, 5 * 60, "VO₂Max"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "5x4 Intervals",
                "Five 4-minute VO₂max intervals.",
                Vo2Max,
                chain(&[
                    vec![wu(10)],
                    ivls(5, 4 * 60, 112.0, 4 * 60, "VO₂Max"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "VO₂Max Long",
                "Three 6-minute efforts at 115% FTP.",
                Vo2Max,
                chain(&[
                    vec![wu(10)],
                    ivls(3, 6 * 60, 115.0, 5 * 60, "VO₂Max"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "VO₂Max Blocks",
                "Two sets of three 3-minute VO₂max intervals.",
                Vo2Max,
                chain(&[
                    vec![wu(10)],
                    ivls(3, 3 * 60, 118.0, 3 * 60, "VO₂Max"),
                    vec![rv(5)],
                    ivls(3, 3 * 60, 118.0, 3 * 60, "VO₂Max"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "VO₂Max Staircase",
                "Ascending VO₂max efforts from 106 to 122% FTP.",
                Vo2Max,
                vec![
                    wu(15),
                    st(3 * 60, 106.0, "Step 1"),
                    rv(2),
                    st(3 * 60, 110.0, "Step 2"),
                    rv(2),
                    st(3 * 60, 114.0, "Step 3"),
                    rv(2),
                    st(3 * 60, 118.0, "Step 4"),
                    rv(2),
                    st(3 * 60, 122.0, "Step 5"),
                    cd(10),
                ],
            ),
            mk(
                "Tabata Protocol",
                "Two Tabata sets (8×20 s/10 s) at 150% FTP.",
                Vo2Max,
                chain(&[
                    vec![wu(15)],
                    ivls(8, 20, 150.0, 10, "Tabata"),
                    vec![rv(3)],
                    ivls(8, 20, 150.0, 10, "Tabata"),
                    vec![cd(15)],
                ]),
            ),
            mk(
                "30-30 Intervals",
                "Twenty 30-second VO₂max efforts with equal rest.",
                Vo2Max,
                chain(&[
                    vec![wu(10)],
                    ivls(20, 30, 130.0, 30, "Effort"),
                    vec![rv(5), cd(10)],
                ]),
            ),
            mk(
                "VO₂Max Sustained",
                "Three 5-minute VO₂max efforts.",
                Vo2Max,
                chain(&[
                    vec![wu(10)],
                    ivls(3, 5 * 60, 110.0, 5 * 60, "VO₂Max"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "Pyramid Efforts",
                "1-2-3-2-1 minute VO₂max pyramid.",
                Vo2Max,
                vec![
                    wu(15),
                    st(60, 115.0, "1 min"),
                    rv(2),
                    st(2 * 60, 115.0, "2 min"),
                    rv(3),
                    st(3 * 60, 115.0, "3 min"),
                    rv(3),
                    st(2 * 60, 115.0, "2 min"),
                    rv(2),
                    st(60, 115.0, "1 min"),
                    cd(15),
                ],
            ),
            mk(
                "Micro-Bursts",
                "Two sets of 10 micro-burst efforts at 150% FTP.",
                Vo2Max,
                chain(&[
                    vec![wu(10)],
                    ivls(10, 15, 150.0, 15, "Burst"),
                    vec![rv(5)],
                    ivls(10, 15, 150.0, 15, "Burst"),
                    vec![cd(10)],
                ]),
            ),
            mk(
                "VO₂Max Classic",
                "Five 4-minute efforts — a VO₂max training staple.",
                Vo2Max,
                chain(&[
                    vec![wu(10)],
                    ivls(5, 4 * 60, 110.0, 4 * 60, "VO₂Max"),
                    vec![cd(10)],
                ]),
            ),
            // ── Anaerobic (10) ─────────────────────────────────────────────────
            mk(
                "10x1 Anaerobic",
                "Ten 1-minute all-out efforts to build anaerobic capacity.",
                Anaerobic,
                chain(&[
                    vec![wu(15)],
                    ivls(10, 60, 150.0, 2 * 60, "Anaerobic"),
                    vec![cd(15)],
                ]),
            ),
            mk(
                "8x1 Hard",
                "Eight maximal 1-minute efforts at 165% FTP.",
                Anaerobic,
                chain(&[
                    vec![wu(15)],
                    ivls(8, 60, 165.0, 2 * 60, "Hard"),
                    vec![cd(15)],
                ]),
            ),
            mk(
                "Sprint Training",
                "Ten 20-second sprint efforts at maximum power.",
                Anaerobic,
                chain(&[
                    vec![wu(15)],
                    ivls(10, 20, 175.0, 2 * 60, "Sprint"),
                    vec![cd(15)],
                ]),
            ),
            mk(
                "12x30s Anaerobic",
                "Twelve 30-second efforts at 150% FTP.",
                Anaerobic,
                chain(&[
                    vec![wu(15)],
                    ivls(12, 30, 150.0, 2 * 60, "Anaerobic"),
                    vec![cd(15)],
                ]),
            ),
            mk(
                "Neuromuscular Power",
                "Eight 20-second maximal efforts to develop peak power.",
                Anaerobic,
                chain(&[
                    vec![wu(15)],
                    ivls(8, 20, 175.0, 2 * 60, "NM Power"),
                    vec![cd(15)],
                ]),
            ),
            mk(
                "Anaerobic Capacity",
                "Twelve 30-second high-power efforts with short rest.",
                Anaerobic,
                chain(&[
                    vec![wu(15)],
                    ivls(12, 30, 150.0, 90, "Anaerobic Cap"),
                    vec![cd(15)],
                ]),
            ),
            mk(
                "MAP Intervals",
                "Six 90-second efforts at your maximal aerobic power.",
                Anaerobic,
                chain(&[
                    vec![wu(15)],
                    ivls(6, 90, 135.0, 3 * 60, "MAP"),
                    vec![cd(15)],
                ]),
            ),
            mk(
                "Attacking Race Sim",
                "Five 2-minute attacking efforts — simulates race surges.",
                Anaerobic,
                chain(&[
                    vec![wu(15)],
                    ivls(5, 2 * 60, 140.0, 3 * 60, "Attack"),
                    vec![cd(15)],
                ]),
            ),
            mk(
                "Championship Efforts",
                "Four 2.5-minute maximal efforts.",
                Anaerobic,
                chain(&[
                    vec![wu(15)],
                    ivls(4, 150, 135.0, 3 * 60, "Championship"),
                    vec![cd(15)],
                ]),
            ),
            mk(
                "Alactic Power",
                "Six 15-second all-out sprints — trains the phosphocreatine system.",
                Anaerobic,
                chain(&[
                    vec![wu(15)],
                    ivls(6, 15, 175.0, 3 * 60, "Alactic"),
                    vec![cd(15)],
                ]),
            ),
            // ── FTP Tests (2) ─────────────────────────────────────────────────
            mk(
                "Ramp Test",
                "Incremental ramp to exhaustion. FTP = last completed minute × 0.75. \
                 Each minute the target rises by ~8% FTP. Go until you can no longer hold \
                 the required power for 15 seconds.",
                Custom,
                chain(&[
                    vec![wu(10)],
                    vec![
                        st(60, 60.0, "Ramp 60%"),
                        st(60, 68.0, "Ramp 68%"),
                        st(60, 76.0, "Ramp 76%"),
                        st(60, 84.0, "Ramp 84%"),
                        st(60, 92.0, "Ramp 92%"),
                        st(60, 100.0, "Ramp 100%"),
                        st(60, 108.0, "Ramp 108%"),
                        st(60, 116.0, "Ramp 116%"),
                        st(60, 124.0, "Ramp 124%"),
                        st(60, 132.0, "Ramp 132%"),
                        st(60, 140.0, "Ramp 140%"),
                        st(60, 148.0, "Ramp 148%"),
                    ],
                    vec![cd(10)],
                ]),
            ),
            mk(
                "20-Minute FTP Test",
                "Gold-standard FTP test. Ride the 20-minute all-out section as hard as \
                 you can sustain. FTP = 20-minute average power × 0.95.",
                Custom,
                vec![
                    wu(10),
                    st(5 * 60, 90.0, "Pre-load"),
                    rv(5),
                    st(20 * 60, 105.0, "20-Minute Effort"),
                    cd(10),
                ],
            ),
        ]
    }
}

// ── Private workout-builder helpers ──────────────────────────────────────────

fn wu(mins: u32) -> Segment {
    Segment::ramp(mins * 60, 40.0, 60.0, "Warm-up")
}

fn cd(mins: u32) -> Segment {
    Segment::steady(mins * 60, 40.0, "Cool-down")
}

fn rv(mins: u32) -> Segment {
    Segment::steady(mins * 60, 50.0, "Recovery")
}

fn st(secs: u32, pct: f32, label: &str) -> Segment {
    Segment::steady(secs, pct, label)
}

fn ivls(n: u32, on_secs: u32, on_pct: f32, off_secs: u32, label: &str) -> Vec<Segment> {
    let mut v = Vec::new();
    for i in 1..=n {
        v.push(Segment::steady(on_secs, on_pct, &format!("{label} {i}")));
        if i < n {
            v.push(Segment::steady(off_secs, 50.0, "Recovery"));
        }
    }
    v
}

fn chain(parts: &[Vec<Segment>]) -> Vec<Segment> {
    parts.iter().flatten().cloned().collect()
}

fn mk(name: &str, desc: &str, cat: WorkoutCategory, segs: Vec<Segment>) -> Workout {
    Workout::from_segments(name, desc, cat, segs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_score_an_hour_at_ftp_as_one_hundred_tss() {
        let seg = Segment::steady(3600, 100.0, "Threshold");
        assert!((seg.tss_contribution() - 100.0).abs() < 0.01);
    }

    #[test]
    fn should_score_an_hour_at_half_ftp_as_a_quarter_of_that() {
        // TSS goes with the square of intensity, not with intensity.
        let seg = Segment::steady(3600, 50.0, "Recovery");
        assert!((seg.tss_contribution() - 25.0).abs() < 0.01);
    }

    #[test]
    fn should_score_a_ramp_at_its_midpoint() {
        let ramp = Segment::ramp(3600, 50.0, 150.0, "Warmup");
        let steady = Segment::steady(3600, 100.0, "Same average");
        assert!((ramp.tss_contribution() - steady.tss_contribution()).abs() < 0.01);
    }

    #[test]
    fn should_score_a_zero_length_segment_as_nothing() {
        assert_eq!(Segment::steady(0, 100.0, "Empty").tss_contribution(), 0.0);
    }

    #[test]
    fn should_sum_duration_and_tss_across_segments() {
        let w = Workout::from_segments(
            "Test",
            "",
            WorkoutCategory::Threshold,
            vec![
                Segment::steady(1800, 100.0, "On"),
                Segment::steady(1800, 50.0, "Off"),
            ],
        );
        assert_eq!(w.duration_secs, 3600);
        // Half an hour at FTP (50) plus half an hour at half FTP (12.5).
        assert!((w.tss - 62.5).abs() < 0.01, "{}", w.tss);
        assert_eq!(w.id, 0, "an unsaved workout has no id yet");
    }

    #[test]
    fn should_build_an_empty_workout_without_dividing_by_zero() {
        let w = Workout::from_segments("Empty", "", WorkoutCategory::Custom, Vec::new());
        assert_eq!(w.duration_secs, 0);
        assert_eq!(w.tss, 0.0);
        assert_eq!(w.average_ftp_percent(), 0.0);
    }
}
