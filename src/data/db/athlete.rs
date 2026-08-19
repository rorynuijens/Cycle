//! The athlete row, and the FTP history that records how it changed.
//!
//! The FTP history is an audit trail of every change, consumed by FTP detection
//! (docs/ftp-detection.md) for cooldown logic and the "Last updated" subtitle in
//! Preferences.

use crate::data::athlete::AthleteProfile;
use anyhow::Result;
use sqlx::{Row, SqlitePool};

/// Load the athlete profile, creating a default one if the table is empty.
pub async fn load_or_create_athlete(pool: &SqlitePool) -> Result<AthleteProfile> {
    let row = sqlx::query(
        "SELECT id, name, weight_kg, ftp_watts, max_hr, resting_hr FROM athletes LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;

    if let Some(r) = row {
        return Ok(AthleteProfile {
            id: r.get("id"),
            name: r.get("name"),
            weight_kg: r.get::<f64, _>("weight_kg") as f32,
            ftp_watts: r.get::<i64, _>("ftp_watts") as u32,
            max_hr: r.get::<i64, _>("max_hr") as u32,
            resting_hr: r.get::<i64, _>("resting_hr") as u32,
        });
    }

    let athlete = AthleteProfile::default();
    let result = sqlx::query(
        "INSERT INTO athletes (name, weight_kg, ftp_watts, max_hr, resting_hr)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&athlete.name)
    .bind(athlete.weight_kg as f64)
    .bind(athlete.ftp_watts as i64)
    .bind(athlete.max_hr as i64)
    .bind(athlete.resting_hr as i64)
    .execute(pool)
    .await?;

    Ok(AthleteProfile {
        id: result.last_insert_rowid(),
        ..athlete
    })
}

/// Record an FTP change. `source` is one of "manual", "suggestion", "ramp_test".
pub async fn log_ftp_change(
    pool: &SqlitePool,
    ftp_watts: u32,
    source: &str,
    note: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO ftp_history (date, ftp_watts, source, note)
         VALUES (datetime('now'), ?, ?, ?)",
    )
    .bind(ftp_watts as i64)
    .bind(source)
    .bind(note)
    .execute(pool)
    .await?;
    Ok(())
}

/// Most recent FTP history entry as `(date, ftp_watts, source)`, if any.
// TODO(ftp-detect): used by the phase-2 check-in card for cooldown decisions.
// Phase-1 `ftp_history` CRUD from docs/ftp-detection.md §10 — caller-less by
// design until the detector ships, not an oversight.
#[allow(dead_code)]
pub async fn latest_ftp_entry(pool: &SqlitePool) -> Result<Option<(String, u32, String)>> {
    let row = sqlx::query(
        "SELECT date, ftp_watts, source FROM ftp_history
         ORDER BY date DESC, id DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| {
        (
            r.get::<String, _>("date"),
            r.get::<i64, _>("ftp_watts") as u32,
            r.get::<String, _>("source"),
        )
    }))
}

/// Persist changes to the (single) athlete profile row.
pub async fn update_athlete(pool: &SqlitePool, athlete: &AthleteProfile) -> Result<()> {
    let result = sqlx::query(
        "UPDATE athletes
         SET name = ?, weight_kg = ?, ftp_watts = ?, max_hr = ?, resting_hr = ?
         WHERE id = ?",
    )
    .bind(&athlete.name)
    .bind(athlete.weight_kg as f64)
    .bind(athlete.ftp_watts as i64)
    .bind(athlete.max_hr as i64)
    .bind(athlete.resting_hr as i64)
    .bind(athlete.id)
    .execute(pool)
    .await?;

    // A mismatched id is not an sqlx error — the UPDATE simply matches nothing
    // and reports success, which is indistinguishable from a real save at the
    // call site. Say so loudly instead.
    if result.rows_affected() == 0 {
        tracing::warn!(
            "update_athlete matched no row (id={}); profile was not persisted",
            athlete.id
        );
    }
    Ok(())
}

/// Wipe athlete profile and, optionally, all training data.
///
/// API keys (`anthropic.api_key`, `intervals.api_key`, `intervals.athlete_id`,
/// `intervals.upload`, `intervals.sync`) and window geometry are always preserved.
/// Saved BLE devices are always preserved.
pub async fn reset_athlete_data(pool: &SqlitePool, include_training_data: bool) -> Result<()> {
    // Always: remove the athlete row (will be recreated as default on next load)
    sqlx::query("DELETE FROM athletes").execute(pool).await?;

    if include_training_data {
        sqlx::query("DELETE FROM sessions").execute(pool).await?;
        sqlx::query("DELETE FROM calendar_entries")
            .execute(pool)
            .await?;
        sqlx::query("DELETE FROM workouts").execute(pool).await?;
        sqlx::query("DELETE FROM intervals_activities")
            .execute(pool)
            .await?;
        sqlx::query("DELETE FROM intervals_workouts")
            .execute(pool)
            .await?;
        sqlx::query("DELETE FROM activity_streams")
            .execute(pool)
            .await?;
        sqlx::query("DELETE FROM wellness_entries")
            .execute(pool)
            .await?;
        sqlx::query("DELETE FROM athlete_goals")
            .execute(pool)
            .await?;
        sqlx::query("DELETE FROM time_off_entries")
            .execute(pool)
            .await?;
        sqlx::query(
            "DELETE FROM settings WHERE key NOT IN (
                'anthropic.api_key',
                'intervals.api_key',
                'intervals.athlete_id',
                'intervals.upload',
                'intervals.sync',
                'window.width',
                'window.height'
            )",
        )
        .execute(pool)
        .await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::db::testing::*;

    #[tokio::test]
    async fn load_or_create_athlete_creates_default_then_reuses_it() {
        let pool = test_pool().await;

        let created = load_or_create_athlete(&pool).await.unwrap();
        assert!(created.id > 0);
        assert_eq!(created.ftp_watts, AthleteProfile::default().ftp_watts);

        // Second call must return the same row, not insert another athlete.
        let reloaded = load_or_create_athlete(&pool).await.unwrap();
        assert_eq!(reloaded.id, created.id);

        let count: i64 = sqlx::query("SELECT COUNT(*) AS c FROM athletes")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("c");
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn update_athlete_round_trips() {
        let pool = test_pool().await;
        let mut athlete = load_or_create_athlete(&pool).await.unwrap();
        athlete.ftp_watts = 312;
        athlete.weight_kg = 68.5;
        update_athlete(&pool, &athlete).await.unwrap();

        let reloaded = load_or_create_athlete(&pool).await.unwrap();
        assert_eq!(reloaded.ftp_watts, 312);
        assert!((reloaded.weight_kg - 68.5).abs() < 0.01);
    }

    #[tokio::test]
    async fn should_not_report_success_when_the_athlete_id_matches_no_row() {
        // The onboarding wizard degrades to id = 0 if the initial load fails.
        // UPDATE ... WHERE id = 0 touches nothing but still returns Ok, which
        // is what made a failed save indistinguishable from a real one.
        let pool = test_pool().await;
        let real = load_or_create_athlete(&pool).await.unwrap();

        let ghost = AthleteProfile {
            id: 0,
            ftp_watts: 999,
            ..real.clone()
        };
        update_athlete(&pool, &ghost).await.unwrap();

        // The stored row must be untouched by a write aimed at a missing id.
        let reloaded = load_or_create_athlete(&pool).await.unwrap();
        assert_eq!(reloaded.id, real.id);
        assert_eq!(reloaded.ftp_watts, real.ftp_watts);
        assert_ne!(reloaded.ftp_watts, 999);
    }

    #[tokio::test]
    async fn ftp_history_logs_and_returns_latest() {
        let pool = test_pool().await;
        assert!(latest_ftp_entry(&pool).await.unwrap().is_none());

        log_ftp_change(&pool, 250, "manual", "").await.unwrap();
        log_ftp_change(&pool, 260, "suggestion", "check-in 2026-07")
            .await
            .unwrap();

        let (_date, ftp, source) = latest_ftp_entry(&pool).await.unwrap().unwrap();
        assert_eq!(ftp, 260);
        assert_eq!(source, "suggestion");
    }
}
