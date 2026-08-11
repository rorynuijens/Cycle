//! Scheduled workouts: the plan, and what today holds.

use crate::data::workout::{Segment, Workout, WorkoutCategory};
use anyhow::Result;
use sqlx::{Row, SqlitePool};

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct CalendarEntry {
    pub id: i64,
    pub workout_id: i64,
    pub workout_name: String,
    /// ISO date string "YYYY-MM-DD"
    pub scheduled_date: String,
    pub completed: bool,
    pub category: WorkoutCategory,
    pub tss: f32,
    pub duration_secs: u32,
}

/// Load all calendar entries for a given month, joined with workout names.
#[allow(dead_code)]
pub async fn load_calendar_entries_for_month(
    pool: &SqlitePool,
    year: i32,
    month: u32,
) -> Result<Vec<CalendarEntry>> {
    let pattern = format!("{:04}-{:02}-%", year, month);
    let rows = sqlx::query(
        "SELECT ce.id, ce.workout_id, w.name AS workout_name,
                ce.scheduled_date, ce.completed,
                w.category, w.tss, w.duration_secs
         FROM calendar_entries ce
         JOIN workouts w ON ce.workout_id = w.id
         WHERE ce.scheduled_date LIKE ?
         ORDER BY ce.scheduled_date",
    )
    .bind(&pattern)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| CalendarEntry {
            id: r.get("id"),
            workout_id: r.get("workout_id"),
            workout_name: r.get("workout_name"),
            scheduled_date: r.get("scheduled_date"),
            completed: r.get::<i64, _>("completed") != 0,
            category: WorkoutCategory::from_db_str(&r.get::<String, _>("category")),
            tss: r.get::<f32, _>("tss"),
            duration_secs: r.get::<i64, _>("duration_secs") as u32,
        })
        .collect())
}

/// Mark all incomplete calendar entries for a given workout and date as done.
pub async fn complete_today_calendar_entry(
    pool: &SqlitePool,
    workout_id: i64,
    date: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE calendar_entries SET completed = 1
         WHERE workout_id = ? AND scheduled_date = ? AND completed = 0",
    )
    .bind(workout_id)
    .bind(date)
    .execute(pool)
    .await?;
    Ok(())
}

/// Load calendar entries whose `scheduled_date` falls within [start_date, end_date] inclusive.
pub async fn load_calendar_entries_between(
    pool: &SqlitePool,
    start_date: &str,
    end_date: &str,
) -> Result<Vec<CalendarEntry>> {
    let rows = sqlx::query(
        "SELECT ce.id, ce.workout_id, w.name AS workout_name,
                ce.scheduled_date, ce.completed,
                w.category, w.tss, w.duration_secs
         FROM calendar_entries ce
         JOIN workouts w ON ce.workout_id = w.id
         WHERE ce.scheduled_date >= ? AND ce.scheduled_date <= ?
         ORDER BY ce.scheduled_date",
    )
    .bind(start_date)
    .bind(end_date)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| CalendarEntry {
            id: r.get("id"),
            workout_id: r.get("workout_id"),
            workout_name: r.get("workout_name"),
            scheduled_date: r.get("scheduled_date"),
            completed: r.get::<i64, _>("completed") != 0,
            category: WorkoutCategory::from_db_str(&r.get::<String, _>("category")),
            tss: r.get::<f32, _>("tss"),
            duration_secs: r.get::<i64, _>("duration_secs") as u32,
        })
        .collect())
}

/// Count incomplete calendar entries from `from_date` (ISO "YYYY-MM-DD") onward.
pub async fn count_upcoming_scheduled(pool: &SqlitePool, from_date: &str) -> Result<i64> {
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM calendar_entries WHERE scheduled_date >= ? AND completed = 0",
    )
    .bind(from_date)
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// Insert a calendar entry scheduling a workout on a given ISO date ("YYYY-MM-DD").
///
/// `program_id` names the training program this session belongs to, or `None`
/// for one the rider scheduled themselves or accepted from a daily suggestion.
/// Program adaptation only ever touches rows that carry an id, which is what
/// keeps it away from everything else on the calendar.
pub async fn schedule_workout(
    pool: &SqlitePool,
    workout_id: i64,
    date: &str,
    program_id: Option<i64>,
) -> Result<i64> {
    let result = sqlx::query(
        "INSERT INTO calendar_entries (workout_id, scheduled_date, completed, program_id)
         VALUES (?, ?, 0, ?)",
    )
    .bind(workout_id)
    .bind(date)
    .bind(program_id)
    .execute(pool)
    .await?;
    Ok(result.last_insert_rowid())
}

/// A workout scheduled for a specific day, with its completion state.
pub struct TodayEntry {
    pub workout: Workout,
    pub completed: bool,
}

/// Load the first calendar entry for the given ISO date, preferring incomplete ones.
pub async fn load_today_entry(pool: &SqlitePool, date: &str) -> Result<Option<TodayEntry>> {
    let row = sqlx::query(
        "SELECT ce.completed, w.id, w.name, w.description, w.duration_secs,
                w.tss, w.category, w.segments_json
         FROM calendar_entries ce
         JOIN workouts w ON ce.workout_id = w.id
         WHERE ce.scheduled_date = ?
         ORDER BY ce.completed ASC, ce.id ASC
         LIMIT 1",
    )
    .bind(date)
    .fetch_optional(pool)
    .await?;

    match row {
        None => Ok(None),
        Some(r) => {
            let segments: Vec<Segment> =
                serde_json::from_str(r.get("segments_json")).unwrap_or_default();
            let category_str: String = r.get("category");
            Ok(Some(TodayEntry {
                workout: Workout {
                    id: r.get("id"),
                    name: r.get("name"),
                    description: r.get("description"),
                    duration_secs: r.get::<i64, _>("duration_secs") as u32,
                    tss: r.get::<f64, _>("tss") as f32,
                    category: WorkoutCategory::from_db_str(&category_str),
                    segments,
                },
                completed: r.get::<i64, _>("completed") != 0,
            }))
        }
    }
}

/// Delete all incomplete calendar entries for today matching the given workout.
pub async fn delete_today_calendar_entry(
    pool: &SqlitePool,
    workout_id: i64,
    date: &str,
) -> Result<()> {
    sqlx::query(
        "DELETE FROM calendar_entries WHERE workout_id = ? AND scheduled_date = ? AND completed = 0",
    )
    .bind(workout_id)
    .bind(date)
    .execute(pool)
    .await?;
    Ok(())
}

/// Delete any calendar entry by its primary key (used from the calendar delete button).
pub async fn delete_calendar_entry_by_id(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query("DELETE FROM calendar_entries WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}
