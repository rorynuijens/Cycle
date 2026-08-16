//! Everything the Fitness page reads from the database.
//!
//! Each struct is one page concern, and each loader fetches that concern in a
//! single pass off the GTK main thread (CLAUDE.md §2.3).

use chrono::NaiveDate;
use sqlx::SqlitePool;

use crate::data::{db, settings};
use crate::training::analytics::WELLNESS_WINDOW_DAYS;

/// Everything the page's charts are drawn from, loaded in one pass.
pub struct FitnessData {
    pub records: Vec<db::SessionRecord>,
    pub intervals_pairs: Vec<(NaiveDate, f32)>,
    pub icu_activities: Vec<db::IntervalsActivity>,
    pub wellness: Vec<db::WellnessEntry>,
    pub run_streams: Vec<(NaiveDate, String)>,
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
    })
}

/// The training history behind a retrospective prompt.
pub struct RetroPromptData {
    /// Sessions inside the retrospective period.
    pub records: Vec<db::SessionRecord>,
    pub icu_acts: Vec<db::IntervalsActivity>,
    pub intervals_all: Vec<(NaiveDate, f32)>,
    pub wellness: Vec<db::WellnessEntry>,
    /// Every ride ever, as summaries — the fitness trend needs history from
    /// before the period. Summaries, not records: the trend only reads stored
    /// metrics, and loading each ride's samples to compute figures the columns
    /// already hold costs half a megabyte of JSON per riding hour.
    pub all_rides: Vec<db::SessionSummary>,
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
        all_rides: db::load_session_summaries(pool).await?,
        athlete_context: settings::coaching_context(pool).await?,
    })
}
