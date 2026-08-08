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
                resting_hr: r.get::<Option<i64>, _>("resting_hr").map(|v| v as u32),
                sleep_secs: r.get::<Option<i64>, _>("sleep_secs").map(|v| v as u32),
                sleep_score: r.get::<Option<i64>, _>("sleep_score").map(|v| v as u32),
                steps: r.get::<Option<i64>, _>("steps").map(|v| v as u32),
                calories: r.get::<Option<i64>, _>("calories").map(|v| v as u32),
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
                resting_hr: r.get::<Option<i64>, _>("resting_hr").map(|v| v as u32),
                sleep_secs: r.get::<Option<i64>, _>("sleep_secs").map(|v| v as u32),
                sleep_score: r.get::<Option<i64>, _>("sleep_score").map(|v| v as u32),
                steps: r.get::<Option<i64>, _>("steps").map(|v| v as u32),
                calories: r.get::<Option<i64>, _>("calories").map(|v| v as u32),
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

#[allow(dead_code)]
pub async fn load_upcoming_time_off(pool: &SqlitePool, limit: u32) -> Result<Vec<TimeOffEntry>> {
    let today = chrono::Local::now()
        .date_naive()
        .format("%Y-%m-%d")
        .to_string();
    let rows = sqlx::query(
        "SELECT date, notes FROM time_off_entries WHERE date >= ? ORDER BY date LIMIT ?",
    )
    .bind(&today)
    .bind(limit as i64)
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
