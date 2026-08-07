//! Everything the Fitness page reads from the database.
//!
//! Each struct is one page concern, and each loader fetches that concern in a
//! single pass off the GTK main thread (CLAUDE.md §2.3).

use chrono::NaiveDate;
use sqlx::SqlitePool;

use crate::data::db;
use crate::training::analytics::WELLNESS_WINDOW_DAYS;

/// Everything the page's charts are drawn from, loaded in one pass.
pub struct FitnessData {
    pub records: Vec<db::SessionRecord>,
    pub intervals_pairs: Vec<(NaiveDate, f32)>,
    pub icu_activities: Vec<db::IntervalsActivity>,
    pub wellness: Vec<db::WellnessEntry>,
    pub run_streams: Vec<(NaiveDate, String)>,
    pub cached_insight: String,
}

/// Load the page's data off the GTK main thread (CLAUDE.md §2.3).
///
/// Every query hits the same local database, so the first failure aborts the
/// whole load rather than leaving the page part-drawn from stale data.
pub async fn load_fitness_data(pool: &SqlitePool) -> anyhow::Result<FitnessData> {
    Ok(FitnessData {
        records: db::load_session_records(pool).await?,
        intervals_pairs: db::load_intervals_tss_pairs(pool).await?,
        icu_activities: db::load_unlinked_intervals_activities(pool).await?,
        wellness: db::load_wellness_recent(pool, WELLNESS_WINDOW_DAYS as u32).await?,
        run_streams: db::load_run_activity_streams(pool).await?,
        cached_insight: db::get_setting(pool, "ai.fitness_insight")
            .await?
            .unwrap_or_default(),
    })
}

/// Days of wellness history sent with the "Analyse Fitness" prompt.
const AI_WELLNESS_DAYS: u32 = 7;

/// The training history behind the "Analyse Fitness" prompt.
pub struct FitnessPromptData {
    pub records: Vec<db::SessionSummary>,
    pub intervals_pairs: Vec<(NaiveDate, f32)>,
    pub icu_count: usize,
    pub wellness: Vec<db::WellnessEntry>,
    pub athlete_context: String,
}

/// Load the history the fitness analysis is based on.
///
/// Unlike the chart data, a partial read here is not a cosmetic problem: the
/// prompt would still be sent, and the AI would confidently analyse a training
/// history that is missing rides. The first failure aborts the request.
pub async fn load_fitness_prompt_data(pool: &SqlitePool) -> anyhow::Result<FitnessPromptData> {
    Ok(FitnessPromptData {
        records: db::load_session_summaries(pool).await?,
        intervals_pairs: db::load_intervals_tss_pairs(pool).await?,
        icu_count: db::count_intervals_activities(pool).await? as usize,
        wellness: db::load_wellness_recent(pool, AI_WELLNESS_DAYS).await?,
        athlete_context: db::get_setting(pool, "coaching.athlete_context")
            .await?
            .unwrap_or_default(),
    })
}

/// The training history behind a retrospective prompt.
pub struct RetroPromptData {
    /// Sessions inside the retrospective period.
    pub records: Vec<db::SessionRecord>,
    pub icu_acts: Vec<db::IntervalsActivity>,
    pub intervals_all: Vec<(NaiveDate, f32)>,
    pub wellness: Vec<db::WellnessEntry>,
    /// All sessions ever — the fitness trend needs history from before the period.
    pub all_records: Vec<db::SessionRecord>,
    pub athlete_context: String,
}

/// Load the history a retrospective is based on. Aborts on the first failure,
/// for the same reason as [`load_fitness_prompt_data`].
pub async fn load_retro_prompt_data(
    pool: &SqlitePool,
    start_utc: &str,
    end_utc: &str,
    start_date: NaiveDate,
    today: NaiveDate,
) -> anyhow::Result<RetroPromptData> {
    Ok(RetroPromptData {
        records: db::load_sessions_between(pool, start_utc, end_utc).await?,
        icu_acts: db::load_unlinked_intervals_activities(pool).await?,
        intervals_all: db::load_intervals_tss_pairs(pool).await?,
        wellness: db::load_wellness_between(
            pool,
            &start_date.format("%Y-%m-%d").to_string(),
            &today.format("%Y-%m-%d").to_string(),
        )
        .await?,
        all_records: db::load_session_records(pool).await?,
        athlete_context: db::get_setting(pool, "coaching.athlete_context")
            .await?
            .unwrap_or_default(),
    })
}
