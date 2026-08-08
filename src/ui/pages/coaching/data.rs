//! Everything the Coaching page reads from the database.
//!
//! Each struct is one page concern, and each loader fetches that concern in a
//! single pass off the GTK main thread (CLAUDE.md §2.3).

use chrono::{Duration as CDuration, NaiveDate};
use sqlx::SqlitePool;

use crate::data::{ai_cache, db, settings};

/// Days of wellness history sent with a coaching prompt.
const AI_WELLNESS_DAYS: u32 = 7;

/// How far ahead the suggestion prompt looks for planned time off.
const TIME_OFF_LOOKAHEAD_DAYS: i64 = 14;

/// The page's own state: the rider's goals and the last cached suggestion.
pub struct CoachingData {
    pub goals: Vec<db::AthleteGoal>,
    pub cached_resp: String,
    pub cached_name: String,
    pub cached_detail: String,
}

/// Load the page's state off the GTK main thread (CLAUDE.md §2.3).
pub async fn load_coaching_data(pool: &SqlitePool) -> anyhow::Result<CoachingData> {
    let cached = ai_cache::load_suggestion(pool).await?;
    Ok(CoachingData {
        goals: db::load_goals(pool).await?,
        cached_resp: cached.response,
        cached_name: cached.workout_name,
        cached_detail: cached.workout_detail,
    })
}

/// The training history behind a "what should I ride today?" prompt.
pub struct SuggestionPromptData {
    pub athlete_ctx: String,
    pub records: Vec<db::SessionSummary>,
    pub intervals_pairs: Vec<(NaiveDate, f32)>,
    pub icu_activities: Vec<db::IntervalsActivity>,
    pub goals: Vec<db::AthleteGoal>,
    pub icu_workouts: Vec<db::IntervalsWorkout>,
    pub wellness: Vec<db::WellnessEntry>,
    pub time_off: Vec<db::TimeOffEntry>,
}

/// Load the history a workout suggestion is based on.
///
/// The first failure aborts: a partial read would still be sent, and the coach
/// would recommend a session having been shown a training history that is
/// missing rides — at the rider's expense, since the request is billed.
pub async fn load_suggestion_prompt_data(
    pool: &SqlitePool,
    today: NaiveDate,
) -> anyhow::Result<SuggestionPromptData> {
    let lookahead = today + CDuration::days(TIME_OFF_LOOKAHEAD_DAYS);
    Ok(SuggestionPromptData {
        athlete_ctx: settings::coaching_context(pool).await?,
        records: db::load_session_summaries(pool).await?,
        intervals_pairs: db::load_intervals_tss_pairs(pool).await?,
        icu_activities: db::load_unlinked_intervals_activities(pool).await?,
        goals: db::load_goals(pool).await?,
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

/// The training history behind a multi-week program prompt.
pub struct ProgramPromptData {
    pub athlete_ctx: String,
    pub goals: Vec<db::AthleteGoal>,
    pub records: Vec<db::SessionSummary>,
    pub intervals_pairs: Vec<(NaiveDate, f32)>,
    pub icu_workouts: Vec<db::IntervalsWorkout>,
}

/// Load the history a training program is built from. Aborts on the first
/// failure, for the same reason as [`load_suggestion_prompt_data`].
pub async fn load_program_prompt_data(pool: &SqlitePool) -> anyhow::Result<ProgramPromptData> {
    Ok(ProgramPromptData {
        athlete_ctx: settings::coaching_context(pool).await?,
        goals: db::load_goals(pool).await?,
        records: db::load_session_summaries(pool).await?,
        intervals_pairs: db::load_intervals_tss_pairs(pool).await?,
        icu_workouts: db::load_intervals_workouts(pool).await?,
    })
}
