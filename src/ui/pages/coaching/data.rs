//! Everything the Coaching page reads from the database.
//!
//! Each struct is one page concern, and each loader fetches that concern in a
//! single pass off the GTK main thread (CLAUDE.md §2.3).

use chrono::{Duration as CDuration, NaiveDate};
use sqlx::SqlitePool;

use crate::data::{db, settings};

/// Days of wellness history sent with a coaching prompt.
const AI_WELLNESS_DAYS: u32 = 7;

/// How far ahead a program looks for planned time off.
///
/// Much further than a daily suggestion needs: a plan being laid out for the
/// next several months should know about a holiday in week six, which a
/// fortnight's view would place it straight on top of.
const PROGRAM_TIME_OFF_LOOKAHEAD_DAYS: i64 = 180;

/// The page's own state: the rider's goals and the last cached suggestion.
pub struct CoachingData {
    pub goals: Vec<db::AthleteGoal>,
    /// The rider's Intervals.icu templates, so a brief naming one can still be
    /// described here even though it cannot be started here.
    pub icu_workouts: Vec<db::IntervalsWorkout>,
}

/// Load the page's state off the GTK main thread (CLAUDE.md §2.3).
pub async fn load_coaching_data(pool: &SqlitePool) -> anyhow::Result<CoachingData> {
    Ok(CoachingData {
        goals: db::load_goals(pool).await?,
        icu_workouts: db::load_intervals_workouts(pool).await?,
    })
}

/// Everything the "Your Program" card needs to describe the plan and work out
/// whether anything should change about it.
pub struct PlanData {
    /// `None` when the rider is not following a program.
    pub program: Option<crate::training::program::Program>,
    pub sessions: Vec<crate::training::program::PlannedSession>,
    /// The days real training happened on, over the whole history.
    ///
    /// The program uses this to tell a day the rider rode through from one they
    /// skipped, without asking them (see
    /// [`crate::training::matching::trained_days`]).
    pub trained: std::collections::HashSet<NaiveDate>,
    pub metrics: crate::training::fitness::LoadMetrics,
    pub wellness: Vec<db::WellnessEntry>,
    /// Scheduled workouts belonging to no program: first, last, and how many.
    /// These predate program tracking and can be adopted.
    pub orphans: Option<(NaiveDate, NaiveDate, i64)>,
}

/// Load the program's state. `fallback_ftp` scores only rides that carry no
/// stamped FTP of their own, matching every other load calculation.
pub async fn load_plan_data(
    pool: &SqlitePool,
    today: NaiveDate,
    fallback_ftp: u32,
) -> anyhow::Result<PlanData> {
    let program = db::active_program(pool).await?;
    let sessions = match &program {
        Some(p) => db::load_program_sessions(pool, p.id).await?,
        None => Vec::new(),
    };

    // Form comes from what was actually ridden, not from what was planned, so
    // it reads the same history the Fitness page does.
    let records = db::load_session_summaries(pool).await?;
    let intervals_pairs = db::load_intervals_tss_pairs(pool).await?;
    // Unlinked, so a ride this app recorded and then uploaded is not read from
    // both sources — harmless for a set of dates, but the same rule everywhere.
    let activities = db::load_unlinked_intervals_activities(pool).await?;
    let trained = crate::training::matching::trained_days(&records, &activities);
    let metrics = crate::training::fitness::compute_load_metrics(
        &records,
        &intervals_pairs,
        fallback_ftp,
        today,
    );

    Ok(PlanData {
        program,
        sessions,
        trained,
        metrics,
        wellness: db::load_wellness_recent(pool, AI_WELLNESS_DAYS.max(14)).await?,
        orphans: db::orphan_entry_span(pool).await?,
    })
}

/// The training history behind a multi-week program prompt.
pub struct ProgramPromptData {
    pub athlete_ctx: String,
    pub goals: Vec<db::AthleteGoal>,
    pub records: Vec<db::SessionSummary>,
    pub intervals_pairs: Vec<(NaiveDate, f32)>,
    pub icu_workouts: Vec<db::IntervalsWorkout>,
    pub wellness: Vec<db::WellnessEntry>,
    pub time_off: Vec<db::TimeOffEntry>,
}

/// Load the history a training program is built from. Aborts on the first
/// failure, for the same reason as [`load_suggestion_prompt_data`].
///
/// Wellness and planned time off are included for the same reason the daily
/// suggestion sends them: a plan laid over a fortnight the rider is away for
/// is a plan they will miss.
pub async fn load_program_prompt_data(
    pool: &SqlitePool,
    today: NaiveDate,
) -> anyhow::Result<ProgramPromptData> {
    let lookahead = today + CDuration::days(PROGRAM_TIME_OFF_LOOKAHEAD_DAYS);
    Ok(ProgramPromptData {
        athlete_ctx: settings::coaching_context(pool).await?,
        goals: db::load_goals(pool).await?,
        records: db::load_session_summaries(pool).await?,
        intervals_pairs: db::load_intervals_tss_pairs(pool).await?,
        icu_workouts: db::load_intervals_workouts(pool).await?,
        wellness: db::load_wellness_recent(pool, AI_WELLNESS_DAYS).await?,
        time_off: db::load_time_off_between(
            pool,
            &today.format("%Y-%m-%d").to_string(),
            &lookahead.format("%Y-%m-%d").to_string(),
        )
        .await?,
    })
}
