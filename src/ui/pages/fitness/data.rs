//! Everything the Fitness page reads from the database.
//!
//! Each struct is one page concern, and each loader fetches that concern in a
//! single pass off the GTK main thread (CLAUDE.md §2.3).

use chrono::NaiveDate;
use sqlx::SqlitePool;

use crate::data::{db, settings};
use crate::training::analytics::{synced_rides, SyncedRide, WELLNESS_WINDOW_DAYS};
use crate::training::ftp_detect::{session_evidence, SessionEvidence, Snooze};

/// Everything the page's charts are drawn from, loaded in one pass.
pub struct FitnessData {
    pub records: Vec<db::SessionRecord>,
    pub intervals_pairs: Vec<(NaiveDate, f32)>,
    pub icu_activities: Vec<db::IntervalsActivity>,
    pub wellness: Vec<db::WellnessEntry>,
    pub run_streams: Vec<(NaiveDate, String)>,
    /// Synced rides with per-second data, already parsed: a summer of outdoor
    /// rides is tens of megabytes of JSON, which must not be read on the GTK
    /// thread.
    pub synced_rides: Vec<SyncedRide>,
    pub check_in: CheckInData,
}

/// What the FTP check-in is judged from. The verdict itself is reached on the
/// main thread, against the FTP in force when the page is drawn.
pub struct CheckInData {
    /// Every in-app ride that has evidence to give, already reduced: reading
    /// it means parsing each ride's samples, which stays off the GTK thread.
    pub evidence: Vec<SessionEvidence>,
    /// The day of the most recent FTP change, whatever made it.
    pub last_change: Option<NaiveDate>,
    pub snooze: Option<Snooze>,
}

/// Reduce rides to check-in evidence. `planned` gives a workout's planned
/// length by id, which is how a ride stopped part-way through an interval is
/// told from one that was finished.
fn check_in_evidence(
    records: &[db::SessionRecord],
    planned: &std::collections::HashMap<i64, u32>,
) -> Vec<SessionEvidence> {
    records
        .iter()
        .filter_map(|r| {
            let duration = r
                .session
                .workout_id
                .and_then(|id| planned.get(&id).copied());
            session_evidence(&r.session, duration)
        })
        .collect()
}

/// Load the page's data off the GTK main thread (CLAUDE.md §2.3).
///
/// Every query hits the same local database, so the first failure aborts the
/// whole load rather than leaving the page part-drawn from stale data.
pub async fn load_fitness_data(pool: &SqlitePool) -> anyhow::Result<FitnessData> {
    let icu_activities = db::load_unlinked_intervals_activities(pool).await?;
    let ftp_history = db::load_ftp_history(pool).await?;
    let synced_rides = synced_rides(
        &icu_activities,
        &db::load_ride_activity_streams(pool).await?,
        &ftp_history,
    );
    let records = db::load_session_records(pool).await?;
    let planned = db::load_workouts(pool)
        .await?
        .into_iter()
        .map(|w| (w.id, w.duration_secs))
        .collect();
    let check_in = CheckInData {
        evidence: check_in_evidence(&records, &planned),
        // Oldest first, so the last entry is the latest change.
        last_change: ftp_history.last().map(|(day, _)| *day),
        snooze: settings::ftp_check_in_snooze(pool).await?,
    };
    Ok(FitnessData {
        records,
        intervals_pairs: db::load_intervals_tss_pairs(pool).await?,
        icu_activities,
        wellness: db::load_wellness_recent(pool, WELLNESS_WINDOW_DAYS as u32).await?,
        run_streams: db::load_run_activity_streams(pool).await?,
        synced_rides,
        check_in,
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
