//! A training program the rider is actually living with.
//!
//! The plan builder produces weeks of sessions and puts them on the calendar.
//! This module is the other half: what has happened to that plan since, and
//! what — if anything — should change about the part that has not happened yet.
//!
//! Everything here is deterministic and local. The rules cost nothing to run,
//! can be tested without a network, and each one has to be defensible as
//! coaching rather than merely as code. The AI rebuild is a separate, billed
//! path for when the rider wants the whole remainder reconsidered.
//!
//! Two principles run through it:
//!
//! * **A missed session is gone.** Nothing is ever moved or stacked to catch
//!   up. Training load is a rolling average, and riding two hard days back to
//!   back to repay a debt is how people get hurt.
//! * **Adjustments only ever ease.** No rule here adds work. The worst case for
//!   easing a session wrongly is a slightly light day; the worst case for
//!   adding one is an injury.

use chrono::NaiveDate;

use crate::data::db::WellnessEntry;
use crate::data::workout::{Workout, WorkoutCategory};
use crate::training::analytics::build_wellness_series;
use crate::training::fitness::{LoadMetrics, TsbBand};

/// Weeks in a block: three building, then one easier.
const BLOCK_WEEKS: u32 = 4;

/// Consecutive missed sessions before the next one is eased.
///
/// One missed session is life. Two in a row means the adaptation the next hard
/// session assumes was never made.
const MISSED_RUN_FOR_EASING: usize = 2;

/// How far resting heart rate must sit above its own recent average to read as
/// a body under strain rather than day-to-day variation.
///
/// Deliberately stricter than the 3 % the wellness card uses to colour a trend:
/// that threshold answers "is this worth showing you", this one answers "is
/// this worth changing your training over".
const RESTING_HR_ELEVATION: f32 = 0.05;

/// A sleep score at or below this reads as a bad night rather than a normal one.
const POOR_SLEEP_SCORE: f32 = 50.0;

/// Readings needed before a wellness baseline means anything.
const MIN_WELLNESS_READINGS: usize = 4;

/// A program the rider is following.
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub id: i64,
    pub start_monday: NaiveDate,
    pub num_weeks: u32,
    /// The days the plan was built around, as stored. Shown, not computed on.
    pub training_days: String,
}

/// One session the program put on the calendar.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedSession {
    pub entry_id: i64,
    pub date: NaiveDate,
    pub workout_id: i64,
    pub workout_name: String,
    pub category: WorkoutCategory,
    pub tss: f32,
    pub duration_secs: u32,
    pub completed: bool,
    /// The workout the program originally asked for, when this entry has
    /// already been eased.
    pub adjusted_from: Option<String>,
}

/// Where a week sits in the build/recover cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Build,
    Recovery,
}

impl Phase {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Build => "Build",
            Self::Recovery => "Recovery",
        }
    }
}

/// The program as it actually stands today.
#[derive(Debug, Clone, PartialEq)]
pub struct ProgramStatus {
    /// 1-based, clamped into the program's span.
    pub week: u32,
    pub total_weeks: u32,
    pub phase: Phase,
    pub planned: usize,
    pub completed: usize,
    /// Past sessions that were never ridden, oldest first.
    pub missed: Vec<PlannedSession>,
    /// Sessions still to come, soonest first.
    pub upcoming: Vec<PlannedSession>,
}

impl ProgramStatus {
    /// How many of the most recent decided sessions were missed in a row.
    ///
    /// Counts backwards from the last session whose day has passed, so a rider
    /// who missed two a fortnight ago and has ridden everything since scores
    /// zero.
    pub fn missed_run(&self, decided: &[PlannedSession]) -> usize {
        decided.iter().rev().take_while(|s| !s.completed).count()
    }
}

/// Why a session is being eased. Each variant carries what it was measured
/// from, so the card can show the rider the number behind the advice.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Reason {
    /// Form is dug in.
    Fatigued { tsb: f64 },
    /// Resting heart rate is up on its own baseline, or sleep was poor.
    WellnessDip { resting_hr_pct: Option<f32> },
    /// Sessions were missed back to back.
    MissedRun { count: usize },
}

impl Reason {
    /// The sentence shown under the adjustment, addressed to the rider.
    pub fn text(&self) -> String {
        match self {
            Self::Fatigued { tsb } => format!(
                "Your form is {tsb:+.0}. A hard session on top of that buys fatigue, not fitness."
            ),
            Self::WellnessDip {
                resting_hr_pct: Some(pct),
            } => format!(
                "Your resting heart rate is {pct:.0}% above its recent average — \
                 a sign your body is still dealing with something."
            ),
            Self::WellnessDip {
                resting_hr_pct: None,
            } => "You slept badly. Quality work on poor sleep tends to be quality \
                  in name only."
                .to_string(),
            Self::MissedRun { count } => format!(
                "You have missed {count} sessions in a row. Easing back in beats \
                 picking up where the plan assumed you would be."
            ),
        }
    }

    /// Which reason wins when several apply at once.
    ///
    /// Measured fatigue outranks a wellness signal, which outranks missed work:
    /// the first is the most direct evidence of accumulated load, the last is
    /// the most easily explained by an ordinary busy week.
    fn severity(&self) -> u8 {
        match self {
            Self::Fatigued { .. } => 3,
            Self::WellnessDip { .. } => 2,
            Self::MissedRun { .. } => 1,
        }
    }
}

/// A proposed change to one upcoming session.
///
/// There is no "drop" or "move" counterpart on purpose — see the module note.
#[derive(Debug, Clone, PartialEq)]
pub struct Adjustment {
    pub entry_id: i64,
    pub date: NaiveDate,
    pub from_workout_id: i64,
    pub from_name: String,
    pub to_workout_id: i64,
    pub to_name: String,
    pub reason: Reason,
}

/// Which week of the program `today` falls in, 1-based and clamped to its span.
///
/// A date before the program starts reads as week 1, and one past the end reads
/// as the final week, so a plan the rider has run past still describes itself
/// rather than reporting a week that does not exist.
pub fn week_of(program: &Program, today: NaiveDate) -> u32 {
    let days = (today - program.start_monday).num_days();
    if days < 0 {
        return 1;
    }
    let week = (days / 7) as u32 + 1;
    week.min(program.num_weeks.max(1))
}

/// Whether a week builds or recovers, on the classic three-up-one-down block.
///
/// Programs shorter than a full block never reach a recovery week, which falls
/// out of the arithmetic rather than needing a case of its own.
pub fn phase_of(week: u32) -> Phase {
    if week > 0 && week.is_multiple_of(BLOCK_WEEKS) {
        Phase::Recovery
    } else {
        Phase::Build
    }
}

/// Sort the program's sessions into what has happened and what has not.
pub fn status(program: &Program, sessions: &[PlannedSession], today: NaiveDate) -> ProgramStatus {
    let mut ordered = sessions.to_vec();
    ordered.sort_by_key(|s| (s.date, s.entry_id));

    let completed = ordered.iter().filter(|s| s.completed).count();
    let missed: Vec<PlannedSession> = ordered
        .iter()
        .filter(|s| s.date < today && !s.completed)
        .cloned()
        .collect();
    let upcoming: Vec<PlannedSession> = ordered
        .iter()
        .filter(|s| s.date >= today && !s.completed)
        .cloned()
        .collect();

    let week = week_of(program, today);
    ProgramStatus {
        week,
        total_weeks: program.num_weeks,
        phase: phase_of(week),
        planned: ordered.len(),
        completed,
        missed,
        upcoming,
    }
}

/// Sessions whose day has passed, oldest first — the ones with a verdict.
pub fn decided(sessions: &[PlannedSession], today: NaiveDate) -> Vec<PlannedSession> {
    let mut past: Vec<PlannedSession> = sessions
        .iter()
        .filter(|s| s.date < today)
        .cloned()
        .collect();
    past.sort_by_key(|s| (s.date, s.entry_id));
    past
}

/// One step down the intensity ladder. `None` at the bottom, and for a workout
/// whose category says nothing about intensity.
pub fn ease(category: WorkoutCategory) -> Option<WorkoutCategory> {
    use WorkoutCategory::*;
    match category {
        Anaerobic => Some(Vo2Max),
        Vo2Max => Some(Threshold),
        Threshold => Some(SweetSpot),
        SweetSpot => Some(Tempo),
        Tempo => Some(Endurance),
        Endurance => Some(Recovery),
        Recovery | Custom => None,
    }
}

/// True for the categories that demand a rider be ready for them.
pub fn is_hard(category: WorkoutCategory) -> bool {
    matches!(
        category,
        WorkoutCategory::Threshold | WorkoutCategory::Vo2Max | WorkoutCategory::Anaerobic
    )
}

/// The workout in `library` of the given category closest in length to
/// `duration_secs`.
///
/// Length is matched because the rider planned their day around it: a session
/// that eases 60 minutes of threshold into 20 minutes of tempo has solved the
/// intensity problem by inventing a scheduling one.
pub fn pick_replacement(
    library: &[Workout],
    category: WorkoutCategory,
    duration_secs: u32,
) -> Option<&Workout> {
    library
        .iter()
        .filter(|w| w.category == category)
        .min_by_key(|w| w.duration_secs.abs_diff(duration_secs))
}

/// Is resting heart rate up, or sleep poor, enough to ease a session over?
fn wellness_reason(wellness: &[WellnessEntry], today: NaiveDate) -> Option<Reason> {
    let rhr = build_wellness_series(wellness, today, |e| e.resting_hr.map(|v| v as f32));
    let readings: Vec<f32> = rhr.iter().copied().filter(|&v| v > 0.0).collect();

    if readings.len() >= MIN_WELLNESS_READINGS {
        // The baseline excludes today's reading, so a spike is measured against
        // the rider's normal rather than diluted by itself.
        let (latest, earlier) = readings.split_last().expect("length checked above");
        let average = earlier.iter().sum::<f32>() / earlier.len() as f32;
        if average > 0.0 && *latest > average * (1.0 + RESTING_HR_ELEVATION) {
            return Some(Reason::WellnessDip {
                resting_hr_pct: Some((latest - average) / average * 100.0),
            });
        }
    }

    let sleep = build_wellness_series(wellness, today, |e| e.sleep_score.map(|v| v as f32));
    let latest_sleep = sleep.iter().rev().find(|&&v| v > 0.0).copied();
    if latest_sleep.is_some_and(|s| s <= POOR_SLEEP_SCORE) {
        return Some(Reason::WellnessDip {
            resting_hr_pct: None,
        });
    }

    None
}

/// What, if anything, should change about the sessions still to come.
///
/// Returns at most one adjustment — the next session only. Form is recomputed
/// every day, so easing the whole week in advance would be deciding on
/// Wednesday's behalf using Monday's evidence.
pub fn suggest(
    status: &ProgramStatus,
    decided: &[PlannedSession],
    metrics: &LoadMetrics,
    wellness: &[WellnessEntry],
    library: &[Workout],
    today: NaiveDate,
) -> Vec<Adjustment> {
    // A recovery week is already the easy week. There is nothing to take out of
    // it, and nothing may be put in.
    if status.phase == Phase::Recovery {
        return Vec::new();
    }

    let Some(target) = status.upcoming.first() else {
        return Vec::new();
    };

    let tsb = metrics.tsb();
    let mut reasons: Vec<Reason> = Vec::new();
    if TsbBand::of(tsb).is_fatigued() {
        reasons.push(Reason::Fatigued { tsb });
    }
    if let Some(reason) = wellness_reason(wellness, today) {
        reasons.push(reason);
    }
    let run = status.missed_run(decided);
    if run >= MISSED_RUN_FOR_EASING {
        reasons.push(Reason::MissedRun { count: run });
    }

    let Some(reason) = reasons.into_iter().max_by_key(|r| r.severity()) else {
        return Vec::new();
    };

    // Missing sessions is a reason to be careful with hard work, not a reason
    // to downgrade an endurance ride the rider is perfectly able to do.
    if matches!(reason, Reason::MissedRun { .. }) && !is_hard(target.category) {
        return Vec::new();
    }

    let Some(eased) = ease(target.category) else {
        return Vec::new();
    };
    let Some(replacement) = pick_replacement(library, eased, target.duration_secs) else {
        return Vec::new();
    };
    // The library can hold only one workout of a category, and it may be the
    // one already scheduled.
    if replacement.id == target.workout_id {
        return Vec::new();
    }

    vec![Adjustment {
        entry_id: target.entry_id,
        date: target.date,
        from_workout_id: target.workout_id,
        from_name: target.workout_name.clone(),
        to_workout_id: replacement.id,
        to_name: replacement.name.clone(),
        reason,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::workout::Segment;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("hardcoded valid date")
    }

    /// Monday 3 August 2026.
    fn start() -> NaiveDate {
        date(2026, 8, 3)
    }

    fn program(weeks: u32) -> Program {
        Program {
            id: 1,
            start_monday: start(),
            num_weeks: weeks,
            training_days: "monday,wednesday,friday".into(),
        }
    }

    fn session(id: i64, d: NaiveDate, cat: WorkoutCategory, completed: bool) -> PlannedSession {
        PlannedSession {
            entry_id: id,
            date: d,
            workout_id: id * 10,
            workout_name: format!("{} Session", cat.label()),
            category: cat,
            tss: 60.0,
            duration_secs: 3600,
            completed,
            adjusted_from: None,
        }
    }

    fn workout(id: i64, cat: WorkoutCategory, duration_secs: u32) -> Workout {
        Workout {
            id,
            name: format!("{} {}", cat.label(), duration_secs / 60),
            description: String::new(),
            duration_secs,
            tss: 50.0,
            category: cat,
            segments: vec![Segment::steady(duration_secs, 60.0, "Ride")],
        }
    }

    /// A library with one workout per category, all the same length.
    fn library() -> Vec<Workout> {
        use WorkoutCategory::*;
        [
            Recovery, Endurance, Tempo, SweetSpot, Threshold, Vo2Max, Anaerobic,
        ]
        .iter()
        .enumerate()
        .map(|(i, &c)| workout(i as i64 + 1, c, 3600))
        .collect()
    }

    fn metrics(tsb: f64) -> LoadMetrics {
        // TSB is CTL - ATL, so fix CTL and derive the fatigue that yields it.
        LoadMetrics {
            ctl: 50.0,
            atl: 50.0 - tsb,
            ctl_4wk_ago: 50.0,
        }
    }

    fn wellness(days: &[(u32, u32)], from: NaiveDate) -> Vec<WellnessEntry> {
        // (days_ago, resting_hr)
        days.iter()
            .map(|&(ago, rhr)| WellnessEntry {
                date: from - chrono::Duration::days(ago as i64),
                hrv: None,
                resting_hr: Some(rhr),
                sleep_secs: None,
                sleep_score: None,
                steps: None,
                calories: None,
            })
            .collect()
    }

    // ── Weeks and phases ──────────────────────────────────────────────────────

    #[test]
    fn should_call_the_starting_monday_week_one() {
        assert_eq!(week_of(&program(12), start()), 1);
    }

    #[test]
    fn should_keep_the_whole_first_week_in_week_one() {
        // Sunday closes the week it belongs to.
        assert_eq!(week_of(&program(12), date(2026, 8, 9)), 1);
    }

    #[test]
    fn should_advance_a_week_on_the_next_monday() {
        assert_eq!(week_of(&program(12), date(2026, 8, 10)), 2);
    }

    #[test]
    fn should_read_a_date_before_the_start_as_week_one() {
        assert_eq!(week_of(&program(12), date(2026, 7, 30)), 1);
    }

    #[test]
    fn should_clamp_a_date_past_the_end_to_the_final_week() {
        // A plan the rider has run past still describes itself.
        assert_eq!(week_of(&program(4), date(2026, 12, 25)), 4);
    }

    #[test]
    fn should_cross_a_month_boundary_correctly() {
        assert_eq!(week_of(&program(12), date(2026, 9, 1)), 5);
    }

    #[test]
    fn should_make_every_fourth_week_a_recovery_week() {
        assert_eq!(phase_of(4), Phase::Recovery);
        assert_eq!(phase_of(8), Phase::Recovery);
        for week in [1, 2, 3, 5, 6, 7, 9] {
            assert_eq!(phase_of(week), Phase::Build, "week {week}");
        }
    }

    #[test]
    fn should_never_reach_a_recovery_week_in_a_short_program() {
        for week in 1..=3 {
            assert_eq!(phase_of(week), Phase::Build);
        }
    }

    // ── Status ────────────────────────────────────────────────────────────────

    #[test]
    fn should_split_sessions_into_missed_and_upcoming() {
        use WorkoutCategory::*;
        let sessions = vec![
            session(1, date(2026, 8, 3), Endurance, true),
            session(2, date(2026, 8, 5), Threshold, false), // missed
            session(3, date(2026, 8, 7), Tempo, true),
            session(4, date(2026, 8, 10), Vo2Max, false), // upcoming
        ];
        let s = status(&program(12), &sessions, date(2026, 8, 8));

        assert_eq!(s.planned, 4);
        assert_eq!(s.completed, 2);
        assert_eq!(s.missed.len(), 1);
        assert_eq!(s.missed[0].entry_id, 2);
        assert_eq!(s.upcoming.len(), 1);
        assert_eq!(s.upcoming[0].entry_id, 4);
    }

    #[test]
    fn should_treat_todays_unridden_session_as_upcoming_not_missed() {
        // The day is not over — nothing has been missed yet.
        let sessions = vec![session(
            1,
            date(2026, 8, 5),
            WorkoutCategory::Threshold,
            false,
        )];
        let s = status(&program(12), &sessions, date(2026, 8, 5));
        assert!(s.missed.is_empty());
        assert_eq!(s.upcoming.len(), 1);
    }

    #[test]
    fn should_order_upcoming_sessions_soonest_first() {
        use WorkoutCategory::*;
        let sessions = vec![
            session(2, date(2026, 8, 14), Vo2Max, false),
            session(1, date(2026, 8, 10), Tempo, false),
        ];
        let s = status(&program(12), &sessions, date(2026, 8, 8));
        assert_eq!(s.upcoming[0].entry_id, 1);
    }

    #[test]
    fn should_count_a_run_of_missed_sessions_from_the_most_recent() {
        use WorkoutCategory::*;
        let sessions = vec![
            session(1, date(2026, 8, 3), Endurance, false),
            session(2, date(2026, 8, 5), Threshold, true),
            session(3, date(2026, 8, 7), Tempo, false),
            session(4, date(2026, 8, 9), Vo2Max, false),
        ];
        let past = decided(&sessions, date(2026, 8, 10));
        let s = status(&program(12), &sessions, date(2026, 8, 10));
        assert_eq!(s.missed_run(&past), 2, "the two most recent, not all three");
    }

    #[test]
    fn should_score_a_rider_who_is_back_on_it_as_no_run() {
        use WorkoutCategory::*;
        let sessions = vec![
            session(1, date(2026, 8, 3), Endurance, false),
            session(2, date(2026, 8, 5), Threshold, false),
            session(3, date(2026, 8, 7), Tempo, true),
        ];
        let past = decided(&sessions, date(2026, 8, 8));
        let s = status(&program(12), &sessions, date(2026, 8, 8));
        assert_eq!(s.missed_run(&past), 0);
    }

    // ── The ladder ────────────────────────────────────────────────────────────

    #[test]
    fn should_step_down_one_rung_at_a_time() {
        use WorkoutCategory::*;
        assert_eq!(ease(Anaerobic), Some(Vo2Max));
        assert_eq!(ease(Vo2Max), Some(Threshold));
        assert_eq!(ease(Threshold), Some(SweetSpot));
        assert_eq!(ease(SweetSpot), Some(Tempo));
        assert_eq!(ease(Tempo), Some(Endurance));
        assert_eq!(ease(Endurance), Some(Recovery));
    }

    #[test]
    fn should_stop_at_the_bottom_of_the_ladder() {
        assert_eq!(ease(WorkoutCategory::Recovery), None);
    }

    #[test]
    fn should_not_guess_at_a_custom_workouts_intensity() {
        // A custom workout's category says nothing about how hard it is.
        assert_eq!(ease(WorkoutCategory::Custom), None);
    }

    #[test]
    fn should_terminate_when_walked_all_the_way_down() {
        let mut category = WorkoutCategory::Anaerobic;
        let mut steps = 0;
        while let Some(next) = ease(category) {
            category = next;
            steps += 1;
            assert!(steps < 10, "the ladder must not cycle");
        }
        assert_eq!(category, WorkoutCategory::Recovery);
    }

    #[test]
    fn should_pick_the_replacement_closest_in_length() {
        use WorkoutCategory::*;
        let library = vec![
            workout(1, Endurance, 1800),
            workout(2, Endurance, 3600),
            workout(3, Endurance, 7200),
        ];
        let picked = pick_replacement(&library, Endurance, 3300).expect("an endurance workout");
        assert_eq!(picked.duration_secs, 3600);
    }

    #[test]
    fn should_find_nothing_when_the_library_has_no_such_workout() {
        let library = vec![workout(1, WorkoutCategory::Endurance, 3600)];
        assert!(pick_replacement(&library, WorkoutCategory::Tempo, 3600).is_none());
    }

    // ── The rules ─────────────────────────────────────────────────────────────

    /// A program with one hard session coming up and nothing else going on.
    fn ready_status(category: WorkoutCategory, today: NaiveDate) -> ProgramStatus {
        let sessions = vec![session(
            1,
            today + chrono::Duration::days(1),
            category,
            false,
        )];
        status(&program(12), &sessions, today)
    }

    #[test]
    fn should_ease_the_next_session_when_form_is_dug_in() {
        let today = date(2026, 8, 5);
        let s = ready_status(WorkoutCategory::Threshold, today);
        let out = suggest(&s, &[], &metrics(-35.0), &[], &library(), today);

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].entry_id, 1);
        assert!(matches!(out[0].reason, Reason::Fatigued { .. }));
        assert_eq!(out[0].to_name, "Sweet Spot 60");
    }

    #[test]
    fn should_leave_a_rested_rider_alone() {
        let today = date(2026, 8, 5);
        let s = ready_status(WorkoutCategory::Threshold, today);
        assert!(suggest(&s, &[], &metrics(0.0), &[], &library(), today).is_empty());
    }

    #[test]
    fn should_never_touch_a_recovery_week() {
        // Week 4 is already the easy week — nothing comes out of it.
        let today = date(2026, 8, 26); // week 4 of a program starting 3 Aug
        let s = ready_status(WorkoutCategory::Threshold, today);
        assert_eq!(s.phase, Phase::Recovery);
        assert!(suggest(&s, &[], &metrics(-40.0), &[], &library(), today).is_empty());
    }

    #[test]
    fn should_ease_after_two_missed_sessions_in_a_row() {
        use WorkoutCategory::*;
        let today = date(2026, 8, 10);
        let sessions = vec![
            session(1, date(2026, 8, 5), Tempo, false),
            session(2, date(2026, 8, 7), Tempo, false),
            session(3, date(2026, 8, 12), Vo2Max, false),
        ];
        let s = status(&program(12), &sessions, today);
        let past = decided(&sessions, today);
        let out = suggest(&s, &past, &metrics(5.0), &[], &library(), today);

        assert_eq!(out.len(), 1);
        assert!(matches!(out[0].reason, Reason::MissedRun { count: 2 }));
    }

    #[test]
    fn should_not_ease_after_a_single_missed_session() {
        use WorkoutCategory::*;
        let today = date(2026, 8, 10);
        let sessions = vec![
            session(1, date(2026, 8, 5), Tempo, true),
            session(2, date(2026, 8, 7), Tempo, false),
            session(3, date(2026, 8, 12), Vo2Max, false),
        ];
        let s = status(&program(12), &sessions, today);
        let past = decided(&sessions, today);
        assert!(suggest(&s, &past, &metrics(5.0), &[], &library(), today).is_empty());
    }

    #[test]
    fn should_not_downgrade_an_easy_ride_over_missed_sessions() {
        // Missing sessions is a reason to be careful with hard work, not a
        // reason to take an endurance ride away.
        use WorkoutCategory::*;
        let today = date(2026, 8, 10);
        let sessions = vec![
            session(1, date(2026, 8, 5), Tempo, false),
            session(2, date(2026, 8, 7), Tempo, false),
            session(3, date(2026, 8, 12), Endurance, false),
        ];
        let s = status(&program(12), &sessions, today);
        let past = decided(&sessions, today);
        assert!(suggest(&s, &past, &metrics(5.0), &[], &library(), today).is_empty());
    }

    #[test]
    fn should_ease_when_resting_heart_rate_is_up_on_its_baseline() {
        let today = date(2026, 8, 5);
        let s = ready_status(WorkoutCategory::Threshold, today);
        // Steady at 50, then 58 today — 16 % up.
        let w = wellness(&[(4, 50), (3, 50), (2, 50), (1, 50), (0, 58)], today);
        let out = suggest(&s, &[], &metrics(0.0), &w, &library(), today);

        assert_eq!(out.len(), 1);
        assert!(matches!(
            out[0].reason,
            Reason::WellnessDip {
                resting_hr_pct: Some(_)
            }
        ));
    }

    #[test]
    fn should_ignore_normal_day_to_day_variation_in_resting_heart_rate() {
        let today = date(2026, 8, 5);
        let s = ready_status(WorkoutCategory::Threshold, today);
        // 51 against a baseline of 50 — 2 %, inside normal variation.
        let w = wellness(&[(4, 50), (3, 50), (2, 50), (1, 50), (0, 51)], today);
        assert!(suggest(&s, &[], &metrics(0.0), &w, &library(), today).is_empty());
    }

    #[test]
    fn should_want_a_baseline_before_calling_a_reading_elevated() {
        let today = date(2026, 8, 5);
        let s = ready_status(WorkoutCategory::Threshold, today);
        // Two readings is not a baseline, however alarming the second looks.
        let w = wellness(&[(1, 50), (0, 70)], today);
        assert!(suggest(&s, &[], &metrics(0.0), &w, &library(), today).is_empty());
    }

    #[test]
    fn should_let_fatigue_outrank_a_missed_run() {
        use WorkoutCategory::*;
        let today = date(2026, 8, 10);
        let sessions = vec![
            session(1, date(2026, 8, 5), Tempo, false),
            session(2, date(2026, 8, 7), Tempo, false),
            session(3, date(2026, 8, 12), Vo2Max, false),
        ];
        let s = status(&program(12), &sessions, today);
        let past = decided(&sessions, today);
        let out = suggest(&s, &past, &metrics(-35.0), &[], &library(), today);

        assert_eq!(out.len(), 1, "one adjustment, not one per reason");
        assert!(matches!(out[0].reason, Reason::Fatigued { .. }));
    }

    #[test]
    fn should_only_ever_adjust_the_next_session() {
        use WorkoutCategory::*;
        let today = date(2026, 8, 5);
        let sessions = vec![
            session(1, date(2026, 8, 7), Threshold, false),
            session(2, date(2026, 8, 10), Vo2Max, false),
            session(3, date(2026, 8, 12), Anaerobic, false),
        ];
        let s = status(&program(12), &sessions, today);
        let out = suggest(&s, &[], &metrics(-40.0), &[], &library(), today);

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].entry_id, 1, "the soonest one");
    }

    #[test]
    fn should_propose_nothing_when_the_plan_is_finished() {
        let today = date(2026, 8, 5);
        let s = status(&program(12), &[], today);
        assert!(suggest(&s, &[], &metrics(-40.0), &[], &library(), today).is_empty());
    }

    #[test]
    fn should_propose_nothing_when_the_library_cannot_supply_a_replacement() {
        let today = date(2026, 8, 5);
        let s = ready_status(WorkoutCategory::Threshold, today);
        // No sweet spot workout to step down to.
        let sparse = vec![workout(1, WorkoutCategory::Vo2Max, 3600)];
        assert!(suggest(&s, &[], &metrics(-40.0), &[], &sparse, today).is_empty());
    }

    #[test]
    fn should_propose_nothing_when_the_session_is_already_the_easiest_there_is() {
        let today = date(2026, 8, 5);
        let s = ready_status(WorkoutCategory::Recovery, today);
        assert!(suggest(&s, &[], &metrics(-40.0), &[], &library(), today).is_empty());
    }

    #[test]
    fn should_never_propose_swapping_a_workout_for_itself() {
        use WorkoutCategory::*;
        let today = date(2026, 8, 5);
        // The scheduled session *is* the only endurance workout in the library,
        // and easing tempo lands on endurance.
        let mut sessions = vec![session(1, date(2026, 8, 7), Tempo, false)];
        sessions[0].workout_id = 2;
        sessions[0].duration_secs = 3600;
        let s = status(&program(12), &sessions, today);
        let library = vec![workout(2, Endurance, 3600)];
        assert!(suggest(&s, &[], &metrics(-40.0), &[], &library, today).is_empty());
    }
}
