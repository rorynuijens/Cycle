//! Daily rider-logged entries: wellness readings and planned time off.

use anyhow::Result;
use chrono::NaiveDate;
use sqlx::{Row, SqlitePool};

#[derive(Debug, Clone)]
pub struct WellnessEntry {
    pub date: NaiveDate,
    pub hrv: Option<f32>,
    pub resting_hr: Option<u32>,
    pub sleep_secs: Option<u32>,
    pub sleep_score: Option<u32>,
    pub steps: Option<u32>,
    pub calories: Option<u32>,
}

pub async fn upsert_wellness_entry(pool: &SqlitePool, entry: &WellnessEntry) -> Result<()> {
    sqlx::query(
        "INSERT INTO wellness_entries
             (date, hrv, resting_hr, sleep_secs, sleep_score, steps, calories)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(date) DO UPDATE SET
             hrv         = excluded.hrv,
             resting_hr  = excluded.resting_hr,
             sleep_secs  = excluded.sleep_secs,
             sleep_score = excluded.sleep_score,
             steps       = excluded.steps,
             calories    = excluded.calories",
    )
    .bind(entry.date.format("%Y-%m-%d").to_string())
    .bind(entry.hrv.map(|v| v as f64))
    .bind(entry.resting_hr.map(|v| v as i64))
    .bind(entry.sleep_secs.map(|v| v as i64))
    .bind(entry.sleep_score.map(|v| v as i64))
    .bind(entry.steps.map(|v| v as i64))
    .bind(entry.calories.map(|v| v as i64))
    .execute(pool)
    .await?;
    Ok(())
}

/// Load wellness entries between two ISO dates (inclusive), oldest first.
pub async fn load_wellness_between(
    pool: &SqlitePool,
    start: &str,
    end: &str,
) -> Result<Vec<WellnessEntry>> {
    let rows = sqlx::query(
        "SELECT date, hrv, resting_hr, sleep_secs, sleep_score, steps, calories
         FROM wellness_entries WHERE date >= ? AND date <= ? ORDER BY date ASC",
    )
    .bind(start)
    .bind(end)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let date_str: &str = r.get("date");
            let date = NaiveDate::parse_from_str(date_str, "%Y-%m-%d").ok()?;
            Some(WellnessEntry {
                date,
                hrv: r.get::<Option<f64>, _>("hrv").map(|v| v as f32),
                resting_hr: super::stored_count(r.get("resting_hr")),
                sleep_secs: super::stored_count(r.get("sleep_secs")),
                sleep_score: super::stored_count(r.get("sleep_score")),
                steps: super::stored_count(r.get("steps")),
                calories: super::stored_count(r.get("calories")),
            })
        })
        .collect())
}

/// Load the most recent `days` wellness entries, newest first.
pub async fn load_wellness_recent(pool: &SqlitePool, days: u32) -> Result<Vec<WellnessEntry>> {
    let rows = sqlx::query(
        "SELECT date, hrv, resting_hr, sleep_secs, sleep_score, steps, calories
         FROM wellness_entries ORDER BY date DESC LIMIT ?",
    )
    .bind(days as i64)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let date_str: &str = r.get("date");
            let date = NaiveDate::parse_from_str(date_str, "%Y-%m-%d").ok()?;
            Some(WellnessEntry {
                date,
                hrv: r.get::<Option<f64>, _>("hrv").map(|v| v as f32),
                resting_hr: super::stored_count(r.get("resting_hr")),
                sleep_secs: super::stored_count(r.get("sleep_secs")),
                sleep_score: super::stored_count(r.get("sleep_score")),
                steps: super::stored_count(r.get("steps")),
                calories: super::stored_count(r.get("calories")),
            })
        })
        .collect())
}

#[derive(Debug, Clone)]
pub struct TimeOffEntry {
    pub date: NaiveDate,
    /// Free text the rider typed, shown on the calendar's time-off row.
    pub notes: String,
}

pub async fn save_time_off(pool: &SqlitePool, date: NaiveDate, notes: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO time_off_entries (date, notes) VALUES (?, ?)
         ON CONFLICT(date) DO UPDATE SET notes = excluded.notes",
    )
    .bind(date.format("%Y-%m-%d").to_string())
    .bind(notes)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_time_off(pool: &SqlitePool, date: NaiveDate) -> Result<()> {
    sqlx::query("DELETE FROM time_off_entries WHERE date = ?")
        .bind(date.format("%Y-%m-%d").to_string())
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn load_time_off_between(
    pool: &SqlitePool,
    start: &str,
    end: &str,
) -> Result<Vec<TimeOffEntry>> {
    let rows = sqlx::query(
        "SELECT date, notes FROM time_off_entries WHERE date >= ? AND date <= ? ORDER BY date",
    )
    .bind(start)
    .bind(end)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let date = NaiveDate::parse_from_str(r.get("date"), "%Y-%m-%d").ok()?;
            Some(TimeOffEntry {
                date,
                notes: r.get::<String, _>("notes"),
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::db::testing::test_pool;

    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, d).expect("hardcoded valid date")
    }

    fn reading(d: u32, resting_hr: u32) -> WellnessEntry {
        WellnessEntry {
            date: day(d),
            hrv: Some(62.5),
            resting_hr: Some(resting_hr),
            sleep_secs: Some(27_000),
            sleep_score: Some(81),
            steps: Some(9_412),
            calories: Some(2_350),
        }
    }

    fn dates(list: &[WellnessEntry]) -> Vec<u32> {
        use chrono::Datelike;
        list.iter().map(|e| e.date.day()).collect()
    }

    // ── Wellness readings ────────────────────────────────────────────────────

    #[tokio::test]
    async fn should_read_back_every_field_it_wrote() {
        let pool = test_pool().await;
        upsert_wellness_entry(&pool, &reading(9, 48)).await.unwrap();
        let e = &load_wellness_recent(&pool, 7).await.unwrap()[0];
        assert_eq!(e.date, day(9));
        assert_eq!(e.hrv, Some(62.5));
        assert_eq!(e.resting_hr, Some(48));
        assert_eq!(e.sleep_secs, Some(27_000));
        assert_eq!(e.sleep_score, Some(81));
        assert_eq!(e.steps, Some(9_412));
        assert_eq!(e.calories, Some(2_350));
    }

    #[tokio::test]
    async fn should_keep_one_reading_a_day_and_take_the_latest() {
        let pool = test_pool().await;
        upsert_wellness_entry(&pool, &reading(9, 48)).await.unwrap();
        upsert_wellness_entry(&pool, &reading(9, 52)).await.unwrap();
        let all = load_wellness_recent(&pool, 30).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].resting_hr, Some(52));
    }

    #[tokio::test]
    async fn should_clear_a_figure_the_watch_no_longer_reports() {
        let pool = test_pool().await;
        upsert_wellness_entry(&pool, &reading(9, 48)).await.unwrap();
        let mut later = reading(9, 48);
        later.hrv = None;
        upsert_wellness_entry(&pool, &later).await.unwrap();
        assert_eq!(load_wellness_recent(&pool, 1).await.unwrap()[0].hrv, None);
    }

    #[tokio::test]
    async fn should_give_exactly_the_most_recent_days_newest_first() {
        let pool = test_pool().await;
        for d in [3, 9, 5, 7, 1] {
            upsert_wellness_entry(&pool, &reading(d, 50)).await.unwrap();
        }
        assert_eq!(
            dates(&load_wellness_recent(&pool, 3).await.unwrap()),
            [9, 7, 5]
        );
        assert!(load_wellness_recent(&pool, 0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn should_include_both_ends_of_a_range_oldest_first() {
        let pool = test_pool().await;
        for d in [4, 5, 8, 11, 12] {
            upsert_wellness_entry(&pool, &reading(d, 50)).await.unwrap();
        }
        let found = load_wellness_between(&pool, "2026-10-05", "2026-10-11")
            .await
            .unwrap();
        assert_eq!(dates(&found), [5, 8, 11]);
    }

    #[tokio::test]
    async fn should_not_read_a_negative_stored_value_as_four_billion() {
        // CLAUDE.md §5.2: a damaged row must not reach the coach as fact. A
        // cast from a negative integer wraps rather than failing, so −5 bpm
        // would read as 4,294,967,291 — and the brief is written from this.
        let pool = test_pool().await;
        upsert_wellness_entry(&pool, &reading(9, 48)).await.unwrap();
        sqlx::query("UPDATE wellness_entries SET resting_hr = -5, steps = -1")
            .execute(&pool)
            .await
            .unwrap();
        for e in [
            &load_wellness_recent(&pool, 1).await.unwrap()[0],
            &load_wellness_between(&pool, "2026-10-09", "2026-10-09")
                .await
                .unwrap()[0],
        ] {
            assert_eq!(e.resting_hr, None);
            assert_eq!(e.steps, None);
            // The rest of the reading is fine and stays.
            assert_eq!(e.sleep_score, Some(81));
        }
    }

    #[tokio::test]
    async fn should_skip_a_reading_with_a_corrupt_date() {
        let pool = test_pool().await;
        upsert_wellness_entry(&pool, &reading(9, 48)).await.unwrap();
        sqlx::query("INSERT INTO wellness_entries (date, resting_hr) VALUES ('not a date', 50)")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(dates(&load_wellness_recent(&pool, 30).await.unwrap()), [9]);
    }

    // ── Time off ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn should_keep_one_entry_a_day_with_the_latest_note() {
        let pool = test_pool().await;
        save_time_off(&pool, day(20), "Travelling").await.unwrap();
        save_time_off(&pool, day(20), "Wedding — İzmir")
            .await
            .unwrap();
        let all = load_time_off_between(&pool, "2026-10-01", "2026-10-31")
            .await
            .unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].notes, "Wedding — İzmir");
    }

    #[tokio::test]
    async fn should_include_both_ends_of_a_time_off_range_in_order() {
        let pool = test_pool().await;
        for d in [12, 4, 11, 5] {
            save_time_off(&pool, day(d), "").await.unwrap();
        }
        let found = load_time_off_between(&pool, "2026-10-05", "2026-10-11")
            .await
            .unwrap();
        let days: Vec<NaiveDate> = found.iter().map(|e| e.date).collect();
        assert_eq!(days, [day(5), day(11)]);
    }

    #[tokio::test]
    async fn should_delete_only_the_day_asked_for() {
        let pool = test_pool().await;
        save_time_off(&pool, day(20), "a").await.unwrap();
        save_time_off(&pool, day(21), "b").await.unwrap();
        delete_time_off(&pool, day(20)).await.unwrap();
        let left = load_time_off_between(&pool, "2026-10-01", "2026-10-31")
            .await
            .unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].date, day(21));
    }
}
