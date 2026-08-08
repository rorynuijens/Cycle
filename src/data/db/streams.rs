//! Cached per-second streams for Intervals.icu activities.

use anyhow::Result;
use chrono::NaiveDate;
use sqlx::{Row, SqlitePool};

/// Return the cached streams JSON for an Intervals.icu activity, or `None` if not cached.
pub async fn get_activity_streams(pool: &SqlitePool, icu_id: &str) -> Result<Option<String>> {
    let row = sqlx::query("SELECT streams_json FROM activity_streams WHERE icu_id = ?")
        .bind(icu_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| r.get::<String, _>("streams_json")))
}

/// Upsert the raw streams JSON for an Intervals.icu activity.
pub async fn save_activity_streams(pool: &SqlitePool, icu_id: &str, json: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO activity_streams (icu_id, streams_json, fetched_at)
         VALUES (?, ?, datetime('now'))
         ON CONFLICT(icu_id) DO UPDATE SET
             streams_json = excluded.streams_json,
             fetched_at   = excluded.fetched_at",
    )
    .bind(icu_id)
    .bind(json)
    .execute(pool)
    .await?;
    Ok(())
}

/// Load cached streams JSON for all Intervals.icu running activities.
/// Returns `(activity_date, streams_json)` pairs for pace-curve computation.
pub async fn load_run_activity_streams(pool: &SqlitePool) -> Result<Vec<(NaiveDate, String)>> {
    let rows = sqlx::query(
        "SELECT a.date, s.streams_json
         FROM activity_streams s
         JOIN intervals_activities a ON a.icu_id = s.icu_id
         WHERE LOWER(a.sport_type) IN
               ('run','virtualrun','trailrun','snowshoe','ultrawalkrun')",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let date = NaiveDate::parse_from_str(r.get("date"), "%Y-%m-%d").ok()?;
            Some((date, r.get::<String, _>("streams_json")))
        })
        .collect())
}
