//! Cached per-second streams for Intervals.icu activities.

use anyhow::Result;
use chrono::NaiveDate;
use sqlx::{Row, SqlitePool};

use crate::data::sport::{is_cycling, is_run};

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

/// Synced rides and runs with nothing cached yet, newest first, at most `limit`.
///
/// Activities linked to a ride recorded in this app are skipped: that ride
/// already has its samples locally, and the synced copy is hidden everywhere.
/// Other sports are skipped because nothing reads their streams.
pub async fn activities_missing_streams(pool: &SqlitePool, limit: usize) -> Result<Vec<String>> {
    let rows = sqlx::query(
        "SELECT a.icu_id, a.sport_type
         FROM intervals_activities a
         WHERE NOT EXISTS (SELECT 1 FROM activity_streams s WHERE s.icu_id = a.icu_id)
           AND NOT EXISTS (SELECT 1 FROM sessions x WHERE x.icu_id = a.icu_id)
         ORDER BY a.date DESC, a.icu_id DESC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter(|r| {
            let sport: String = r.get("sport_type");
            is_cycling(&sport) || is_run(&sport)
        })
        .map(|r| r.get::<String, _>("icu_id"))
        .take(limit)
        .collect())
}

/// Forget every cached stream, so each one is fetched again.
pub async fn clear_activity_streams(pool: &SqlitePool) -> Result<()> {
    sqlx::query("DELETE FROM activity_streams")
        .execute(pool)
        .await?;
    Ok(())
}

/// Cached streams for every synced cycling activity, as `(icu_id, json)`.
///
/// Returns linked and duplicate copies too — the caller keeps only the ids it
/// is showing, which is where the one-ride-once rule already lives
/// ([`super::load_unlinked_intervals_activities`]).
pub async fn load_ride_activity_streams(pool: &SqlitePool) -> Result<Vec<(String, String)>> {
    let rows = sqlx::query(
        "SELECT a.icu_id, a.sport_type, s.streams_json
         FROM activity_streams s
         JOIN intervals_activities a ON a.icu_id = s.icu_id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter(|r| is_cycling(&r.get::<String, _>("sport_type")))
        .map(|r| (r.get("icu_id"), r.get("streams_json")))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::super::testing::test_pool;
    use super::super::upsert_intervals_activity;
    use super::*;

    async fn activity(pool: &SqlitePool, icu_id: &str, date: &str, sport: &str) {
        upsert_intervals_activity(
            pool,
            icu_id,
            NaiveDate::parse_from_str(date, "%Y-%m-%d").unwrap(),
            "x",
            None,
            Some(3600),
            Some(150),
            None,
            Some(140),
            None,
            sport,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn should_list_uncached_rides_and_runs_newest_first() {
        let pool = test_pool().await;
        activity(&pool, "i1", "2026-08-01", "Ride").await;
        activity(&pool, "i2", "2026-08-03", "Run").await;
        activity(&pool, "i3", "2026-08-02", "VirtualRide").await;
        activity(&pool, "i4", "2026-08-04", "WeightTraining").await;
        activity(&pool, "i5", "2026-08-05", "Swim").await;
        assert_eq!(
            activities_missing_streams(&pool, 10).await.unwrap(),
            vec!["i2", "i3", "i1"]
        );
    }

    #[tokio::test]
    async fn should_stop_at_the_limit_after_skipping_other_sports() {
        // Five newer swims must not use up a limit of one.
        let pool = test_pool().await;
        activity(&pool, "i1", "2026-08-01", "Ride").await;
        for i in 2..7 {
            activity(&pool, &format!("i{i}"), "2026-08-09", "Swim").await;
        }
        assert_eq!(
            activities_missing_streams(&pool, 1).await.unwrap(),
            vec!["i1"]
        );
        assert!(activities_missing_streams(&pool, 0)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn should_skip_cached_and_linked_activities() {
        let pool = test_pool().await;
        activity(&pool, "cached", "2026-08-01", "Ride").await;
        activity(&pool, "linked", "2026-08-02", "Ride").await;
        activity(&pool, "wanted", "2026-08-03", "Ride").await;
        // An empty array is what the backfill stores for "Intervals.icu has
        // none" — it must count as cached, or the ride is asked for forever.
        save_activity_streams(&pool, "cached", "[]").await.unwrap();
        sqlx::query(
            "INSERT INTO sessions (started_at, icu_id) VALUES ('2026-08-02T08:00:00Z', 'linked')",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            activities_missing_streams(&pool, 10).await.unwrap(),
            vec!["wanted"]
        );
    }

    #[tokio::test]
    async fn should_clear_every_cached_stream() {
        let pool = test_pool().await;
        activity(&pool, "i1", "2026-08-01", "Ride").await;
        save_activity_streams(&pool, "i1", "[]").await.unwrap();
        clear_activity_streams(&pool).await.unwrap();
        assert_eq!(
            activities_missing_streams(&pool, 10).await.unwrap(),
            vec!["i1"]
        );
    }

    #[tokio::test]
    async fn should_load_only_cycling_streams() {
        let pool = test_pool().await;
        activity(&pool, "ride", "2026-08-01", "GravelRide").await;
        activity(&pool, "run", "2026-08-02", "Run").await;
        save_activity_streams(&pool, "ride", "[1]").await.unwrap();
        save_activity_streams(&pool, "run", "[2]").await.unwrap();
        assert_eq!(
            load_ride_activity_streams(&pool).await.unwrap(),
            vec![("ride".to_string(), "[1]".to_string())]
        );
    }
}
