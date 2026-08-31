//! Everything the brief is written from, and how to tell when it has moved.
//!
//! Two jobs that belong together. [`load_brief_input`] is the single read that
//! replaced three overlapping ones — the briefing, the ride suggestion and the
//! fitness insight each used to load their own near-identical slice of the same
//! database, which is how they ended up disagreeing.
//!
//! [`BriefInputs`] is the other half: a scalar summary of that read, rendered
//! to a string and stored beside the brief. Comparing it later answers "has
//! anything the coach was told changed?" without asking the coach anything, so
//! a card can say it has been overtaken without spending the rider's money to
//! find out.

use chrono::{DateTime, Duration as CDuration, NaiveDate, Utc};
use sqlx::SqlitePool;

use super::prompt::{PlannedWorkout, TodayPlan};
use crate::data::{
    athlete::AthleteProfile,
    db::{self},
    settings,
    workout::Workout,
};
use crate::training::program::{PlannedSession, Program};

/// Days of wellness history the brief is given.
///
/// Seven rather than one: the first entry is this morning, which the readiness
/// section reads, and the rest are the week the form section interprets. The
/// old morning briefing asked for a single day and could not see a trend.
pub const WELLNESS_DAYS: u32 = 7;

/// How far ahead the brief looks for planned time off.
pub const TIME_OFF_LOOKAHEAD_DAYS: i64 = 14;

/// Weeks of TSS totals shown to the coach.
pub const TSS_WEEKS: i64 = 6;

/// Everything one brief is written from, read in a single pass.
///
/// The first failure aborts the lot. A partial read would still be sent, and
/// the coach would write the rider's morning around a training history missing
/// rides — at the rider's expense, since the request is billed.
pub struct BriefInput {
    pub athlete_context: String,
    pub records: Vec<db::SessionSummary>,
    pub intervals_pairs: Vec<(NaiveDate, f32)>,
    pub icu_activities: Vec<db::IntervalsActivity>,
    pub icu_count: i64,
    pub icu_workouts: Vec<db::IntervalsWorkout>,
    pub wellness: Vec<db::WellnessEntry>,
    pub goals: Vec<db::AthleteGoal>,
    pub workouts: Vec<Workout>,
    pub today_entry: Option<db::TodayEntry>,
    pub time_off: Vec<db::TimeOffEntry>,
    /// The program the rider is following, if any. The brief never knew about
    /// programs before; with one running it may no longer pick the session.
    pub program: Option<Program>,
    pub program_sessions: Vec<PlannedSession>,
}

/// Read everything the brief needs.
pub async fn load_brief_input(pool: &SqlitePool, today: NaiveDate) -> anyhow::Result<BriefInput> {
    let lookahead = today + CDuration::days(TIME_OFF_LOOKAHEAD_DAYS);
    let program = db::active_program(pool).await?;
    let program_sessions = match &program {
        Some(p) => db::load_program_sessions(pool, p.id).await?,
        None => Vec::new(),
    };

    Ok(BriefInput {
        athlete_context: settings::coaching_context(pool).await?,
        records: db::load_session_summaries(pool).await?,
        intervals_pairs: db::load_intervals_tss_pairs(pool).await?,
        icu_activities: db::load_unlinked_intervals_activities(pool).await?,
        icu_count: db::count_intervals_activities(pool).await?,
        icu_workouts: db::load_intervals_workouts(pool).await?,
        wellness: db::load_wellness_recent(pool, WELLNESS_DAYS).await?,
        goals: db::load_goals(pool).await?,
        workouts: db::load_workouts(pool).await?,
        today_entry: db::load_today_entry(pool, &today.format("%Y-%m-%d").to_string()).await?,
        time_off: db::load_time_off_between(
            pool,
            &today.format("%Y-%m-%d").to_string(),
            &lookahead.format("%Y-%m-%d").to_string(),
        )
        .await?,
        program,
        program_sessions,
    })
}

/// The fingerprint of the database as it stands now.
///
/// Used to ask "has anything moved since the brief was written?" — on every
/// page navigation, and never with the network.
///
/// This deliberately re-reads everything rather than computing the same summary
/// from a handful of aggregate queries. A `SUM` over training stress would have
/// to restate [`db::SessionSummary::tss`] in SQL, stamped FTP and fallback and
/// all, and two spellings of that formula drifting apart would mark every brief
/// permanently out of date — a worse failure than the reads it would save. Each
/// query here is a scalar projection over a few hundred rows; none of them load
/// a ride's data points.
pub async fn current_fingerprint(
    pool: &SqlitePool,
    athlete: &AthleteProfile,
    today: NaiveDate,
) -> anyhow::Result<String> {
    let input = load_brief_input(pool, today).await?;
    Ok(BriefInputs::of(&input, athlete, today).fingerprint())
}

impl BriefInput {
    /// What the plan says about today — the fact the precedence rule turns on.
    ///
    /// A program running with nothing scheduled today is a planned rest day,
    /// not an open day: the plan decided that, and the brief does not get to
    /// fill it. That is why this cannot be derived from the calendar alone.
    pub fn today_plan(&self, today: NaiveDate) -> TodayPlan {
        let planned = self.today_entry.as_ref().map(|e| PlannedWorkout {
            name: e.workout.name.clone(),
            duration_mins: e.workout.duration_secs / 60,
            tss: e.workout.tss,
            category: e.workout.category.label().to_string(),
        });

        // Only a program that still has sessions to run owns the day. One the
        // rider has ridden past should not silence the coach for ever.
        let program_running =
            self.program.is_some() && self.program_sessions.iter().any(|s| s.date >= today);

        match (planned, program_running) {
            (Some(w), true) => TodayPlan::Programmed(w),
            (Some(w), false) => TodayPlan::Scheduled(w),
            (None, true) => TodayPlan::ProgramRestDay,
            (None, false) => TodayPlan::Open,
        }
    }
}

// ── The fingerprint ───────────────────────────────────────────────────────────

/// Bumped when the fields below change, so an old fingerprint never compares
/// equal to a new one computed from the same data.
const FINGERPRINT_VERSION: u32 = 1;

/// A scalar summary of everything a brief depends on.
///
/// Deliberately small and all-integral: it is compared for equality, and two
/// floats that print the same are worth more here than two that nearly match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BriefInputs {
    pub ride_count: usize,
    pub latest_ride: Option<DateTime<Utc>>,
    /// Rounded, because the question is "did a ride land", not "to the decimal".
    pub total_tss: i64,
    pub icu_count: i64,
    pub latest_icu: Option<NaiveDate>,
    /// This morning's reading: date, HRV, resting HR, sleep seconds, score.
    pub today_wellness: Option<(NaiveDate, i64, i64, i64, i64)>,
    pub ftp_watts: u32,
    /// Kilograms × 10, so a weight edit moves an integer.
    pub weight_dg: u32,
    pub max_hr: u32,
    /// Today's session and whether it is already ridden.
    pub today_workout: Option<(String, bool)>,
    /// Program id, sessions in it, and how many are done.
    pub program: Option<(i64, usize, usize)>,
    /// Whether today is time off, and how many days off are coming.
    pub time_off: (bool, usize),
    pub context_hash: u32,
}

impl BriefInputs {
    /// Summarise a full read.
    ///
    /// Must agree with [`crate::data::db::brief_inputs`], which computes the
    /// same thing the cheap way — there is a test.
    pub fn of(input: &BriefInput, athlete: &AthleteProfile, today: NaiveDate) -> Self {
        let today_wellness = input
            .wellness
            .iter()
            .find(|w| w.date == today)
            .map(wellness_scalars);

        Self {
            ride_count: input.records.len(),
            latest_ride: input.records.iter().map(|r| r.started_at).max(),
            total_tss: input
                .records
                .iter()
                .filter_map(|r| r.tss(athlete.ftp_watts))
                .sum::<f32>()
                .round() as i64,
            icu_count: input.icu_count,
            latest_icu: input.intervals_pairs.iter().map(|(d, _)| *d).max(),
            today_wellness,
            ftp_watts: athlete.ftp_watts,
            weight_dg: (athlete.weight_kg * 10.0).round().max(0.0) as u32,
            max_hr: athlete.max_hr,
            today_workout: input
                .today_entry
                .as_ref()
                .map(|e| (e.workout.name.clone(), e.completed)),
            program: input.program.as_ref().map(|p| {
                (
                    p.id,
                    input.program_sessions.len(),
                    input
                        .program_sessions
                        .iter()
                        .filter(|s| s.completed)
                        .count(),
                )
            }),
            time_off: (
                input.time_off.iter().any(|t| t.date == today),
                input.time_off.len(),
            ),
            context_hash: fnv1a(&input.athlete_context),
        }
    }

    /// Render as a comparable string.
    ///
    /// Readable rather than hashed, because it is stored next to the brief and
    /// read back by a person when a card claims to be out of date and the rider
    /// disagrees. The athlete's free-text background is the exception: it is
    /// folded to a hash so their private notes never land in a settings row.
    pub fn fingerprint(&self) -> String {
        let latest_ride = self
            .latest_ride
            .map(|t| t.to_rfc3339())
            .unwrap_or_else(|| "-".into());
        let latest_icu = self
            .latest_icu
            .map(|d| d.to_string())
            .unwrap_or_else(|| "-".into());
        let wellness = match self.today_wellness {
            Some((d, hrv, rhr, sleep, score)) => format!("{d}:{hrv}:{rhr}:{sleep}:{score}"),
            None => "-".into(),
        };
        let workout = match &self.today_workout {
            Some((name, done)) => format!("{name}:{}", u8::from(*done)),
            None => "-".into(),
        };
        let program = match self.program {
            Some((id, total, done)) => format!("{id}:{total}:{done}"),
            None => "-".into(),
        };

        format!(
            "v{version}|rides:{rides}@{latest_ride}|tss:{tss}|icu:{icu}@{latest_icu}\
             |well:{wellness}|ftp:{ftp}|dg:{weight}|hr:{hr}|cal:{workout}\
             |prog:{program}|off:{off_today}:{off_count}|ctx:{ctx:08x}",
            version = FINGERPRINT_VERSION,
            rides = self.ride_count,
            tss = self.total_tss,
            icu = self.icu_count,
            ftp = self.ftp_watts,
            weight = self.weight_dg,
            hr = self.max_hr,
            off_today = u8::from(self.time_off.0),
            off_count = self.time_off.1,
            ctx = self.context_hash,
        )
    }
}

/// The five numbers a morning's wellness reading comes down to.
///
/// Shared by both constructors so a missing reading is spelled the same way on
/// each path — `-1` rather than a default that a real reading could equal.
pub fn wellness_scalars(w: &db::WellnessEntry) -> (NaiveDate, i64, i64, i64, i64) {
    (
        w.date,
        w.hrv.map(|v| v.round() as i64).unwrap_or(-1),
        w.resting_hr.map(|v| v as i64).unwrap_or(-1),
        w.sleep_secs.map(|v| v as i64).unwrap_or(-1),
        w.sleep_score.map(|v| v as i64).unwrap_or(-1),
    )
}

/// A stable, non-cryptographic fold of free text.
///
/// `DefaultHasher` is deliberately not used: its output is explicitly not
/// guaranteed stable across Rust releases, so a toolchain bump would quietly
/// mark every rider's brief out of date and bill them for a new one.
pub fn fnv1a(s: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in s.as_bytes() {
        hash ^= *byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::workout::{Workout, WorkoutCategory};

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("hardcoded valid date")
    }

    fn today() -> NaiveDate {
        date(2026, 8, 11)
    }

    fn athlete() -> AthleteProfile {
        AthleteProfile::default()
    }

    fn workout(name: &str) -> Workout {
        Workout {
            id: 1,
            name: name.into(),
            description: String::new(),
            duration_secs: 3600,
            tss: 70.0,
            category: WorkoutCategory::Threshold,
            segments: Vec::new(),
        }
    }

    fn summary(id: i64, at: DateTime<Utc>) -> db::SessionSummary {
        db::SessionSummary {
            id,
            started_at: at,
            duration_secs: 3600,
            normalised_power: Some(200.0),
            average_power: Some(195.0),
            kilojoules: 700.0,
            ftp_watts: Some(200),
            rpe: None,
            workout_name: Some("Threshold".into()),
            uploaded_to_icu: false,
            icu_id: None,
        }
    }

    fn at(day: u32) -> DateTime<Utc> {
        date(2026, 8, day)
            .and_hms_opt(9, 0, 0)
            .expect("valid time")
            .and_utc()
    }

    fn input() -> BriefInput {
        BriefInput {
            athlete_context: String::new(),
            records: vec![summary(1, at(10))],
            intervals_pairs: vec![(date(2026, 8, 9), 60.0)],
            icu_activities: Vec::new(),
            icu_count: 1,
            icu_workouts: Vec::new(),
            wellness: Vec::new(),
            goals: Vec::new(),
            workouts: Vec::new(),
            today_entry: None,
            time_off: Vec::new(),
            program: None,
            program_sessions: Vec::new(),
        }
    }

    fn session(date: NaiveDate, completed: bool) -> PlannedSession {
        PlannedSession {
            trained: false,
            entry_id: 1,
            date,
            workout_id: 1,
            workout_name: "Threshold".into(),
            category: WorkoutCategory::Threshold,
            tss: 70.0,
            duration_secs: 3600,
            completed,
            adjusted_from: None,
        }
    }

    fn program() -> Program {
        Program {
            id: 7,
            start_monday: date(2026, 8, 3),
            num_weeks: 12,
            training_days: "monday,wednesday,friday".into(),
        }
    }

    fn fingerprint(input: &BriefInput) -> String {
        BriefInputs::of(input, &athlete(), today()).fingerprint()
    }

    // ── What the plan says about today ───────────────────────────────────────

    #[test]
    fn should_read_an_empty_day_with_no_program_as_open() {
        assert_eq!(input().today_plan(today()), TodayPlan::Open);
    }

    #[test]
    fn should_read_a_programs_empty_day_as_a_planned_rest_day() {
        // The plan decided today was a rest day. That is not a gap for the
        // brief to fill with a workout of its own.
        let mut i = input();
        i.program = Some(program());
        i.program_sessions = vec![session(date(2026, 8, 12), false)];
        assert_eq!(i.today_plan(today()), TodayPlan::ProgramRestDay);
    }

    #[test]
    fn should_read_a_programs_session_as_programmed() {
        let mut i = input();
        i.program = Some(program());
        i.program_sessions = vec![session(today(), false)];
        i.today_entry = Some(db::TodayEntry {
            workout: workout("Threshold 3x12"),
            completed: false,
        });
        match i.today_plan(today()) {
            TodayPlan::Programmed(w) => assert_eq!(w.name, "Threshold 3x12"),
            other => panic!("expected a programmed session, got {other:?}"),
        }
    }

    #[test]
    fn should_read_a_lone_calendar_entry_as_scheduled_not_programmed() {
        let mut i = input();
        i.today_entry = Some(db::TodayEntry {
            workout: workout("Threshold 3x12"),
            completed: false,
        });
        assert!(matches!(i.today_plan(today()), TodayPlan::Scheduled(_)));
        assert!(!i.today_plan(today()).program_active());
    }

    #[test]
    fn should_stop_treating_a_finished_program_as_owning_the_day() {
        // A plan the rider has ridden all the way through must not silence the
        // coach for ever.
        let mut i = input();
        i.program = Some(program());
        i.program_sessions = vec![session(date(2026, 7, 1), true)];
        assert_eq!(i.today_plan(today()), TodayPlan::Open);
    }

    // ── What moves the fingerprint ───────────────────────────────────────────

    #[test]
    fn should_keep_the_fingerprint_when_nothing_the_brief_reads_has_changed() {
        assert_eq!(fingerprint(&input()), fingerprint(&input()));
    }

    #[test]
    fn should_change_the_fingerprint_when_a_ride_is_recorded() {
        let mut after = input();
        after.records.push(summary(2, at(11)));
        assert_ne!(fingerprint(&input()), fingerprint(&after));
    }

    #[test]
    fn should_change_the_fingerprint_when_a_ride_is_rescored() {
        // Same count, same timestamp — only the training stress moved.
        let mut after = input();
        after.records[0].normalised_power = Some(260.0);
        assert_ne!(fingerprint(&input()), fingerprint(&after));
    }

    #[test]
    fn should_change_the_fingerprint_when_this_mornings_wellness_arrives() {
        let mut after = input();
        after.wellness = vec![db::WellnessEntry {
            date: today(),
            hrv: Some(52.0),
            resting_hr: Some(46),
            sleep_secs: Some(27_000),
            sleep_score: Some(84),
            steps: None,
            calories: None,
        }];
        assert_ne!(fingerprint(&input()), fingerprint(&after));
    }

    #[test]
    fn should_ignore_wellness_for_a_day_that_is_not_today() {
        // Yesterday's reading is already in the brief that was written for
        // yesterday. It does not make this morning's stale.
        let mut after = input();
        after.wellness = vec![db::WellnessEntry {
            date: date(2026, 8, 10),
            hrv: Some(52.0),
            resting_hr: Some(46),
            sleep_secs: Some(27_000),
            sleep_score: Some(84),
            steps: None,
            calories: None,
        }];
        assert_eq!(fingerprint(&input()), fingerprint(&after));
    }

    #[test]
    fn should_change_the_fingerprint_when_ftp_is_edited() {
        let before = BriefInputs::of(&input(), &athlete(), today()).fingerprint();
        let raised = AthleteProfile {
            ftp_watts: 265,
            ..athlete()
        };
        assert_ne!(
            before,
            BriefInputs::of(&input(), &raised, today()).fingerprint()
        );
    }

    #[test]
    fn should_change_the_fingerprint_when_weight_moves_by_a_tenth() {
        let before = BriefInputs::of(&input(), &athlete(), today()).fingerprint();
        let lighter = AthleteProfile {
            weight_kg: athlete().weight_kg - 0.1,
            ..athlete()
        };
        assert_ne!(
            before,
            BriefInputs::of(&input(), &lighter, today()).fingerprint()
        );
    }

    #[test]
    fn should_change_the_fingerprint_when_todays_calendar_entry_changes() {
        let mut scheduled = input();
        scheduled.today_entry = Some(db::TodayEntry {
            workout: workout("Threshold 3x12"),
            completed: false,
        });
        assert_ne!(fingerprint(&input()), fingerprint(&scheduled));

        let mut ridden = input();
        ridden.today_entry = Some(db::TodayEntry {
            workout: workout("Threshold 3x12"),
            completed: true,
        });
        assert_ne!(
            fingerprint(&scheduled),
            fingerprint(&ridden),
            "riding it changes the day as much as scheduling it did"
        );
    }

    #[test]
    fn should_change_the_fingerprint_when_the_program_advances() {
        let mut before = input();
        before.program = Some(program());
        before.program_sessions = vec![session(today(), false)];

        let mut after = input();
        after.program = Some(program());
        after.program_sessions = vec![session(today(), true)];

        assert_ne!(fingerprint(&before), fingerprint(&after));
    }

    #[test]
    fn should_change_the_fingerprint_when_time_off_is_booked() {
        let mut after = input();
        after.time_off = vec![db::TimeOffEntry {
            date: date(2026, 8, 20),
            notes: String::new(),
        }];
        assert_ne!(fingerprint(&input()), fingerprint(&after));
    }

    #[test]
    fn should_change_the_fingerprint_when_the_context_is_edited_to_the_same_length() {
        // Length alone would miss this, which is the whole reason it is hashed.
        let mut before = input();
        before.athlete_context = "Knee is sore".into();
        let mut after = input();
        after.athlete_context = "Knee is fine".into();
        assert_ne!(fingerprint(&before), fingerprint(&after));
    }

    // ── The hash ─────────────────────────────────────────────────────────────

    #[test]
    fn should_hash_the_athlete_context_deterministically() {
        // Golden values. If these change, every rider's brief is marked stale
        // once — so they must only change deliberately.
        assert_eq!(fnv1a(""), 0x811c_9dc5);
        assert_eq!(fnv1a("a"), 0xe40c_292c);
        assert_eq!(fnv1a("foobar"), 0xbf9c_f968);
    }

    #[test]
    fn should_hash_text_that_differs_only_in_case_differently() {
        assert_ne!(fnv1a("Rest week"), fnv1a("rest week"));
    }

    // ── The rendered string ──────────────────────────────────────────────────

    #[test]
    fn should_keep_the_athletes_private_notes_out_of_the_fingerprint() {
        // It is stored in a settings row. Their words must not be.
        let mut i = input();
        i.athlete_context = "Recovering from a hernia operation".into();
        let fp = fingerprint(&i);
        assert!(!fp.contains("hernia"));
        assert!(fp.contains("ctx:"));
    }

    #[test]
    fn should_carry_its_version_so_an_old_fingerprint_never_matches_a_new_one() {
        assert!(fingerprint(&input()).starts_with(&format!("v{FINGERPRINT_VERSION}|")));
    }

    // ── Against a real database ──────────────────────────────────────────────

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite should connect");
        crate::data::migrate::run(&pool)
            .await
            .expect("migration should succeed");
        pool
    }

    #[tokio::test]
    async fn should_load_an_empty_brief_input_from_a_fresh_database() {
        // A rider on their first launch still gets a brief, not an error.
        let pool = test_pool().await;
        let input = load_brief_input(&pool, today())
            .await
            .expect("an empty database still reads");

        assert!(input.records.is_empty());
        assert!(input.program.is_none());
        assert_eq!(input.today_plan(today()), TodayPlan::Open);
    }

    #[tokio::test]
    async fn should_produce_a_stable_fingerprint_from_an_unchanged_database() {
        // The whole out-of-date mechanism rests on this: reading twice with
        // nothing in between must not claim the brief has been overtaken.
        let pool = test_pool().await;
        let first = current_fingerprint(&pool, &athlete(), today())
            .await
            .expect("a fingerprint reads");
        let second = current_fingerprint(&pool, &athlete(), today())
            .await
            .expect("and reads again");
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn should_move_the_fingerprint_when_the_database_changes() {
        let pool = test_pool().await;
        let before = current_fingerprint(&pool, &athlete(), today())
            .await
            .expect("a fingerprint reads");

        db::save_time_off(&pool, date(2026, 8, 20), "Away")
            .await
            .expect("booking time off should succeed");

        let after = current_fingerprint(&pool, &athlete(), today())
            .await
            .expect("and reads again");
        assert_ne!(before, after);
    }

    #[test]
    fn should_render_a_missing_reading_distinguishably_from_a_zero_one() {
        let mut measured = input();
        measured.wellness = vec![db::WellnessEntry {
            date: today(),
            hrv: None,
            resting_hr: Some(0),
            sleep_secs: None,
            sleep_score: None,
            steps: None,
            calories: None,
        }];
        let scalars = wellness_scalars(&measured.wellness[0]);
        assert_eq!(scalars.1, -1, "no HRV reading");
        assert_eq!(scalars.2, 0, "a resting HR that was actually zero");
    }
}
