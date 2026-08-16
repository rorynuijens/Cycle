//! The local mirror of Intervals.icu activities and workouts.

use anyhow::Result;

// Lives with the sessions it reads.
use super::linked_icu_ids;
use chrono::{NaiveDate, NaiveDateTime};
use sqlx::{Row, SqlitePool};

/// Intervals.icu activities that no local session already accounts for.
///
/// Anything the app recorded itself is the better copy — it has per-second data,
/// RPE and the plan it was ridden against — so a ride that made the round trip
/// through Garmin or Strava is dropped here and read from the local session
/// instead. Every place that merges the two lists should load through this, or
/// the same ride is shown, counted and fed to the AI twice.
pub async fn load_unlinked_intervals_activities(
    pool: &SqlitePool,
) -> Result<Vec<IntervalsActivity>> {
    let linked = linked_icu_ids(pool).await?;
    let activities = load_intervals_activities(pool).await?;
    // Collapse before filtering, keeping the linked copy: a ride uploaded twice
    // has only one of its copies linked, so filtering first would leave the
    // other standing alongside the local session.
    Ok(
        crate::data::dedupe::collapse_duplicates(activities, |a| linked.contains(&a.icu_id))
            .into_iter()
            .filter(|a| !linked.contains(&a.icu_id))
            .collect(),
    )
}

/// As [`load_unlinked_intervals_activities`], for a date range.
pub async fn load_unlinked_intervals_activities_between(
    pool: &SqlitePool,
    start_date: &str,
    end_date: &str,
) -> Result<Vec<IntervalsActivity>> {
    let linked = linked_icu_ids(pool).await?;
    let activities = load_intervals_activities_between(pool, start_date, end_date).await?;
    Ok(
        crate::data::dedupe::collapse_duplicates(activities, |a| linked.contains(&a.icu_id))
            .into_iter()
            .filter(|a| !linked.contains(&a.icu_id))
            .collect(),
    )
}

/// Upsert an Intervals.icu activity (keyed on icu_id).
#[allow(clippy::too_many_arguments)]
pub async fn upsert_intervals_activity(
    pool: &SqlitePool,
    icu_id: &str,
    date: NaiveDate,
    name: &str,
    tss: Option<f32>,
    duration_secs: Option<u32>,
    average_watts: Option<u32>,
    normalized_watts: Option<u32>,
    average_hr: Option<u32>,
    max_hr: Option<u32>,
    sport_type: &str,
    start_datetime_local: Option<NaiveDateTime>,
    distance_m: Option<f32>,
    elevation_gain_m: Option<f32>,
    average_cadence: Option<f32>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO intervals_activities
             (icu_id, date, name, tss, duration_secs,
              average_watts, normalized_watts, average_hr, max_hr, sport_type,
              start_datetime_local, distance_m, elevation_gain_m, average_cadence)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(icu_id) DO UPDATE SET
             date                 = excluded.date,
             name                 = excluded.name,
             tss                  = excluded.tss,
             duration_secs        = excluded.duration_secs,
             average_watts        = excluded.average_watts,
             normalized_watts     = excluded.normalized_watts,
             average_hr           = excluded.average_hr,
             max_hr               = excluded.max_hr,
             sport_type           = excluded.sport_type,
             start_datetime_local = excluded.start_datetime_local,
             distance_m           = excluded.distance_m,
             elevation_gain_m     = excluded.elevation_gain_m,
             average_cadence      = excluded.average_cadence",
    )
    .bind(icu_id)
    .bind(date.format("%Y-%m-%d").to_string())
    .bind(name)
    .bind(tss.map(|v| v as f64))
    .bind(duration_secs.map(|v| v as i64))
    .bind(average_watts.map(|v| v as f64))
    .bind(normalized_watts.map(|v| v as f64))
    .bind(average_hr.map(|v| v as f64))
    .bind(max_hr.map(|v| v as i64))
    .bind(sport_type)
    .bind(start_datetime_local.map(|dt| dt.format("%Y-%m-%dT%H:%M:%S").to_string()))
    .bind(distance_m.map(|v| v as f64))
    .bind(elevation_gain_m.map(|v| v as f64))
    .bind(average_cadence.map(|v| v as f64))
    .execute(pool)
    .await?;
    Ok(())
}

/// Load (date, tss) pairs from Intervals.icu activities for CTL/ATL calculation.
pub async fn load_intervals_tss_pairs(pool: &SqlitePool) -> Result<Vec<(NaiveDate, f32)>> {
    // Repeated uploads of one ride are collapsed first. Each copy carries the
    // same TSS, so counting them all would credit the rider two or three times
    // for a single session and inflate fitness and fatigue for weeks after.
    //
    // Session-linked activities are deliberately kept: the local ride excludes
    // itself when it has a link, and this is the row that stands in for it.
    let activities = load_intervals_activities(pool).await?;
    Ok(
        crate::data::dedupe::collapse_duplicates(activities, |_| false)
            .into_iter()
            .filter_map(|a| a.tss.map(|tss| (a.date, tss)))
            .collect(),
    )
}

/// A full Intervals.icu activity row, used for display in History and AI coaching.
#[derive(Debug, Clone)]
pub struct IntervalsActivity {
    pub icu_id: String,
    pub date: NaiveDate,
    pub name: String,
    pub tss: Option<f32>,
    pub duration_secs: Option<u32>,
    pub average_watts: Option<u32>,
    pub normalized_watts: Option<u32>,
    pub average_hr: Option<u32>,
    pub max_hr: Option<u32>,
    pub sport_type: String,
    pub start_datetime_local: Option<NaiveDateTime>,
    pub distance_m: Option<f32>,
    pub elevation_gain_m: Option<f32>,
    pub average_cadence: Option<f32>,
}

/// Total count of cached Intervals.icu activities.
pub async fn count_intervals_activities(pool: &SqlitePool) -> Result<i64> {
    // Counts rides, not rows: a ride uploaded twice is still one session, and
    // this figure goes to the AI coach as the athlete's training history.
    let activities = load_intervals_activities(pool).await?;
    Ok(crate::data::dedupe::collapse_duplicates(activities, |_| false).len() as i64)
}

/// Delete a single Intervals.icu activity by its icu_id.
pub async fn delete_intervals_activity(pool: &SqlitePool, icu_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM intervals_activities WHERE icu_id = ?")
        .bind(icu_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Load all Intervals.icu activities, newest first.
pub async fn load_intervals_activities(pool: &SqlitePool) -> Result<Vec<IntervalsActivity>> {
    let rows = sqlx::query(
        "SELECT icu_id, date, name, tss, duration_secs,
                average_watts, normalized_watts, average_hr, max_hr, sport_type,
                start_datetime_local, distance_m, elevation_gain_m, average_cadence
         FROM intervals_activities ORDER BY date DESC",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let date_str: &str = r.get("date");
            let date = NaiveDate::parse_from_str(date_str, "%Y-%m-%d").ok()?;
            let start_datetime_local = r
                .get::<Option<&str>, _>("start_datetime_local")
                .and_then(|s| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").ok());
            Some(IntervalsActivity {
                icu_id: r.get("icu_id"),
                date,
                name: r.get("name"),
                tss: r.get::<Option<f64>, _>("tss").map(|v| v as f32),
                duration_secs: r.get::<Option<i64>, _>("duration_secs").map(|v| v as u32),
                average_watts: r.get::<Option<f64>, _>("average_watts").map(|v| v as u32),
                normalized_watts: r
                    .get::<Option<f64>, _>("normalized_watts")
                    .map(|v| v as u32),
                average_hr: r.get::<Option<f64>, _>("average_hr").map(|v| v as u32),
                max_hr: r.get::<Option<i64>, _>("max_hr").map(|v| v as u32),
                sport_type: r
                    .get::<Option<&str>, _>("sport_type")
                    .unwrap_or("")
                    .to_string(),
                start_datetime_local,
                distance_m: r.get::<Option<f64>, _>("distance_m").map(|v| v as f32),
                elevation_gain_m: r
                    .get::<Option<f64>, _>("elevation_gain_m")
                    .map(|v| v as f32),
                average_cadence: r.get::<Option<f64>, _>("average_cadence").map(|v| v as f32),
            })
        })
        .collect())
}

/// One row of `intervals_workouts`. The columns are carried whole so callers
/// can pick what they need; not every one has a reader today.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct IntervalsWorkout {
    pub id: i64,
    pub icu_id: String,
    pub name: String,
    pub description: String,
    pub duration_secs: Option<u32>,
    pub tss: Option<f32>,
}

/// Upsert an Intervals.icu workout template (keyed on icu_id).
pub async fn upsert_intervals_workout(
    pool: &SqlitePool,
    icu_id: &str,
    name: &str,
    description: &str,
    duration_secs: Option<u32>,
    tss: Option<f32>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO intervals_workouts (icu_id, name, description, duration_secs, tss)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(icu_id) DO UPDATE SET
             name = excluded.name,
             description = excluded.description,
             duration_secs = excluded.duration_secs,
             tss = excluded.tss",
    )
    .bind(icu_id)
    .bind(name)
    .bind(description)
    .bind(duration_secs.map(|v| v as i64))
    .bind(tss.map(|v| v as f64))
    .execute(pool)
    .await?;
    Ok(())
}

/// Load all Intervals.icu workout templates, sorted by name.
pub async fn load_intervals_workouts(pool: &SqlitePool) -> Result<Vec<IntervalsWorkout>> {
    let rows = sqlx::query(
        "SELECT id, icu_id, name, description, duration_secs, tss
         FROM intervals_workouts ORDER BY name",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| IntervalsWorkout {
            id: r.get("id"),
            icu_id: r.get("icu_id"),
            name: r.get("name"),
            description: r.get("description"),
            duration_secs: r.get::<Option<i64>, _>("duration_secs").map(|v| v as u32),
            tss: r.get::<Option<f64>, _>("tss").map(|v| v as f32),
        })
        .collect())
}

/// Delete all cached Intervals.icu workouts (called before a fresh sync).
pub async fn clear_intervals_workouts(pool: &SqlitePool) -> Result<()> {
    sqlx::query("DELETE FROM intervals_workouts")
        .execute(pool)
        .await?;
    Ok(())
}

/// Count of cached Intervals.icu workout templates.
pub async fn count_intervals_workouts(pool: &SqlitePool) -> Result<i64> {
    let row = sqlx::query("SELECT COUNT(*) AS cnt FROM intervals_workouts")
        .fetch_one(pool)
        .await?;
    Ok(row.get("cnt"))
}

/// Load Intervals.icu activities whose date falls within [start_date, end_date] (ISO 8601).
pub async fn load_intervals_activities_between(
    pool: &SqlitePool,
    start_date: &str,
    end_date: &str,
) -> Result<Vec<IntervalsActivity>> {
    let rows = sqlx::query(
        "SELECT icu_id, date, name, tss, duration_secs,
                average_watts, normalized_watts, average_hr, max_hr, sport_type,
                start_datetime_local, distance_m, elevation_gain_m, average_cadence
         FROM intervals_activities WHERE date >= ? AND date <= ? ORDER BY date ASC",
    )
    .bind(start_date)
    .bind(end_date)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let date_str: &str = r.get("date");
            let date = NaiveDate::parse_from_str(date_str, "%Y-%m-%d").ok()?;
            let start_datetime_local = r
                .get::<Option<&str>, _>("start_datetime_local")
                .and_then(|s| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").ok());
            Some(IntervalsActivity {
                icu_id: r.get("icu_id"),
                date,
                name: r.get("name"),
                tss: r.get::<Option<f64>, _>("tss").map(|v| v as f32),
                duration_secs: r.get::<Option<i64>, _>("duration_secs").map(|v| v as u32),
                average_watts: r.get::<Option<f64>, _>("average_watts").map(|v| v as u32),
                normalized_watts: r
                    .get::<Option<f64>, _>("normalized_watts")
                    .map(|v| v as u32),
                average_hr: r.get::<Option<f64>, _>("average_hr").map(|v| v as u32),
                max_hr: r.get::<Option<i64>, _>("max_hr").map(|v| v as u32),
                sport_type: r
                    .get::<Option<&str>, _>("sport_type")
                    .unwrap_or("")
                    .to_string(),
                start_datetime_local,
                distance_m: r.get::<Option<f64>, _>("distance_m").map(|v| v as f32),
                elevation_gain_m: r
                    .get::<Option<f64>, _>("elevation_gain_m")
                    .map(|v| v as f32),
                average_cadence: r.get::<Option<f64>, _>("average_cadence").map(|v| v as f32),
            })
        })
        .collect())
}
