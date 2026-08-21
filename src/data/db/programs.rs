//! Training programs: the plan the rider is following, and the calendar
//! entries that belong to it.
//!
//! A program owns calendar rows rather than duplicating them. Adaptation acts
//! on `calendar_entries.program_id`, which is what keeps it away from anything
//! the rider scheduled by hand or accepted from a daily suggestion.
//!
//! The logic that reads all this lives in [`crate::training::program`].

use anyhow::Result;
use chrono::{NaiveDate, Utc};
use sqlx::{Row, SqlitePool};

use crate::data::workout::WorkoutCategory;
use crate::training::program::{PlannedSession, Program};

/// Persist a new program and return its id. Does not schedule anything — the
/// caller writes the calendar entries against the id this returns.
pub async fn save_program(
    pool: &SqlitePool,
    start_monday: NaiveDate,
    num_weeks: u32,
    training_days: &str,
) -> Result<i64> {
    let result = sqlx::query(
        "INSERT INTO programs (created_at, start_monday, num_weeks, training_days, active)
         VALUES (?, ?, ?, ?, 1)",
    )
    .bind(Utc::now().to_rfc3339())
    .bind(start_monday.format("%Y-%m-%d").to_string())
    .bind(num_weeks as i64)
    .bind(training_days)
    .execute(pool)
    .await?;
    Ok(result.last_insert_rowid())
}

/// The program the rider is currently following, if any.
///
/// Newest wins. Only one should ever be active, and [`save_program`] callers
/// deactivate the previous one, but ordering makes that a preference rather
/// than something a stale row can break.
pub async fn active_program(pool: &SqlitePool) -> Result<Option<Program>> {
    let row = sqlx::query(
        "SELECT id, start_monday, num_weeks, training_days
           FROM programs
          WHERE active = 1
          ORDER BY id DESC
          LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.and_then(|r| {
        let raw: String = r.get("start_monday");
        // A program whose start date will not parse cannot be placed on a
        // calendar at all, so it is reported as no program rather than one
        // silently anchored to today.
        let start_monday = match NaiveDate::parse_from_str(&raw, "%Y-%m-%d") {
            Ok(d) => d,
            Err(e) => {
                tracing::error!("program {}: unreadable start date {raw:?} ({e})", {
                    let id: i64 = r.get("id");
                    id
                });
                return None;
            }
        };
        Some(Program {
            id: r.get("id"),
            start_monday,
            num_weeks: r.get::<i64, _>("num_weeks").max(1) as u32,
            training_days: r.get("training_days"),
        })
    }))
}

/// Stop tracking a program. Its calendar entries are left alone — those rides
/// were still planned, and history should not rewrite itself.
pub async fn deactivate_program(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query("UPDATE programs SET active = 0 WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Every session a program has on the calendar, oldest first.
pub async fn load_program_sessions(
    pool: &SqlitePool,
    program_id: i64,
) -> Result<Vec<PlannedSession>> {
    let rows = sqlx::query(
        "SELECT ce.id, ce.scheduled_date, ce.completed, ce.workout_id,
                w.name AS workout_name, w.category, w.tss, w.duration_secs,
                orig.name AS original_name
           FROM calendar_entries ce
           JOIN workouts w ON ce.workout_id = w.id
           LEFT JOIN workouts orig ON ce.original_workout_id = orig.id
          WHERE ce.program_id = ?
          ORDER BY ce.scheduled_date, ce.id",
    )
    .bind(program_id)
    .fetch_all(pool)
    .await?;

    let mut sessions = Vec::with_capacity(rows.len());
    for r in rows {
        let raw: String = r.get("scheduled_date");
        let Ok(date) = NaiveDate::parse_from_str(&raw, "%Y-%m-%d") else {
            // One unreadable row must not take the whole plan down with it.
            tracing::warn!("calendar entry with unreadable date {raw:?} — skipped");
            continue;
        };
        sessions.push(PlannedSession {
            entry_id: r.get("id"),
            date,
            workout_id: r.get("workout_id"),
            workout_name: r.get("workout_name"),
            category: WorkoutCategory::from_db_str(&r.get::<String, _>("category")),
            tss: r.get::<f32, _>("tss"),
            duration_secs: r.get::<i64, _>("duration_secs") as u32,
            completed: r.get::<i64, _>("completed") != 0,
            adjusted_from: r.get("original_name"),
        });
    }
    Ok(sessions)
}

/// Swap the workout on one calendar entry, remembering what was there.
///
/// `original_workout_id` is written only when it is still empty, so easing a
/// session twice keeps pointing at what the program first asked for rather
/// than at the previous adjustment.
///
/// Returns whether anything actually changed. It can legitimately be `false` —
/// the entry may have been ridden or deleted since the suggestion was drawn,
/// and two surfaces now offer the button — and a caller that reports success
/// regardless would tell the rider their session was eased when it was not.
pub async fn apply_adjustment(
    pool: &SqlitePool,
    entry_id: i64,
    new_workout_id: i64,
) -> Result<bool> {
    let result = sqlx::query(
        "UPDATE calendar_entries
            SET original_workout_id = COALESCE(original_workout_id, workout_id),
                workout_id = ?
          WHERE id = ? AND completed = 0",
    )
    .bind(new_workout_id)
    .bind(entry_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Put a scheduled session back to what the program originally asked for.
///
/// Returns whether anything changed, for the same reason as
/// [`apply_adjustment`].
pub async fn revert_adjustment(pool: &SqlitePool, entry_id: i64) -> Result<bool> {
    let result = sqlx::query(
        "UPDATE calendar_entries
            SET workout_id = original_workout_id,
                original_workout_id = NULL
          WHERE id = ? AND completed = 0 AND original_workout_id IS NOT NULL",
    )
    .bind(entry_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Clear a program's future, unridden sessions, ready for a rebuilt plan.
///
/// Completed entries and anything before `from` are untouched: the past is not
/// up for revision, and neither is work already done.
pub async fn clear_future_sessions(
    pool: &SqlitePool,
    program_id: i64,
    from: NaiveDate,
) -> Result<u64> {
    let result = sqlx::query(
        "DELETE FROM calendar_entries
          WHERE program_id = ? AND completed = 0 AND scheduled_date >= ?",
    )
    .bind(program_id)
    .bind(from.format("%Y-%m-%d").to_string())
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Calendar entries that belong to no program, within a date range.
///
/// These are what the rider scheduled before programs were tracked. Reported so
/// the Coaching page can offer to adopt them rather than showing nothing at all
/// to someone who plainly has a plan.
pub async fn orphan_entry_span(pool: &SqlitePool) -> Result<Option<(NaiveDate, NaiveDate, i64)>> {
    let row = sqlx::query(
        "SELECT MIN(scheduled_date) AS first, MAX(scheduled_date) AS last, COUNT(*) AS n
           FROM calendar_entries
          WHERE program_id IS NULL AND completed = 0",
    )
    .fetch_one(pool)
    .await?;

    let count: i64 = row.get("n");
    if count == 0 {
        return Ok(None);
    }
    let (Some(first), Some(last)) = (
        row.get::<Option<String>, _>("first"),
        row.get::<Option<String>, _>("last"),
    ) else {
        return Ok(None);
    };
    match (
        NaiveDate::parse_from_str(&first, "%Y-%m-%d"),
        NaiveDate::parse_from_str(&last, "%Y-%m-%d"),
    ) {
        (Ok(a), Ok(b)) => Ok(Some((a, b, count))),
        _ => Ok(None),
    }
}

/// Hand every unowned calendar entry to `program_id`.
///
/// Returns how many were adopted. Completed entries are included: a program
/// that cannot see the sessions the rider already rode would report them all as
/// missed.
pub async fn adopt_orphan_entries(pool: &SqlitePool, program_id: i64) -> Result<u64> {
    let result = sqlx::query("UPDATE calendar_entries SET program_id = ? WHERE program_id IS NULL")
        .bind(program_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::db::testing::*;
    use crate::data::db::{save_workout, schedule_workout};

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("hardcoded valid date")
    }

    async fn workout_named(pool: &SqlitePool, name: &str, category: WorkoutCategory) -> i64 {
        let mut w = sample_workout();
        w.name = name.to_string();
        w.category = category;
        save_workout(pool, &w).await.expect("saving a workout")
    }

    async fn program_with_one_session(pool: &SqlitePool) -> (i64, i64, i64) {
        let program = save_program(pool, date(2026, 8, 3), 12, "monday,wednesday")
            .await
            .unwrap();
        let workout = workout_named(pool, "Threshold 4x8", WorkoutCategory::Threshold).await;
        let entry = schedule_workout(pool, workout, "2026-08-05", Some(program))
            .await
            .unwrap();
        (program, workout, entry)
    }

    #[tokio::test]
    async fn should_round_trip_the_active_program() {
        let pool = test_pool().await;
        let id = save_program(&pool, date(2026, 8, 3), 12, "monday,friday")
            .await
            .unwrap();

        let loaded = active_program(&pool).await.unwrap().expect("just saved");
        assert_eq!(loaded.id, id);
        assert_eq!(loaded.start_monday, date(2026, 8, 3));
        assert_eq!(loaded.num_weeks, 12);
        assert_eq!(loaded.training_days, "monday,friday");
    }

    #[tokio::test]
    async fn should_report_no_program_before_one_is_built() {
        let pool = test_pool().await;
        assert!(active_program(&pool).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn should_forget_a_deactivated_program() {
        let pool = test_pool().await;
        let id = save_program(&pool, date(2026, 8, 3), 8, "monday")
            .await
            .unwrap();
        deactivate_program(&pool, id).await.unwrap();
        assert!(active_program(&pool).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn should_prefer_the_newest_of_two_active_programs() {
        let pool = test_pool().await;
        save_program(&pool, date(2026, 6, 1), 4, "monday")
            .await
            .unwrap();
        let newer = save_program(&pool, date(2026, 8, 3), 12, "friday")
            .await
            .unwrap();
        assert_eq!(active_program(&pool).await.unwrap().unwrap().id, newer);
    }

    #[tokio::test]
    async fn should_load_a_programs_sessions_with_their_workouts() {
        let pool = test_pool().await;
        let (program, workout, entry) = program_with_one_session(&pool).await;

        let sessions = load_program_sessions(&pool, program).await.unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].entry_id, entry);
        assert_eq!(sessions[0].workout_id, workout);
        assert_eq!(sessions[0].workout_name, "Threshold 4x8");
        assert_eq!(sessions[0].category, WorkoutCategory::Threshold);
        assert_eq!(sessions[0].date, date(2026, 8, 5));
        assert!(!sessions[0].completed);
        assert_eq!(sessions[0].adjusted_from, None);
    }

    #[tokio::test]
    async fn should_not_see_sessions_belonging_to_another_program() {
        let pool = test_pool().await;
        let (program, _, _) = program_with_one_session(&pool).await;
        let other = save_program(&pool, date(2026, 8, 3), 4, "monday")
            .await
            .unwrap();

        assert_eq!(load_program_sessions(&pool, other).await.unwrap().len(), 0);
        assert_eq!(
            load_program_sessions(&pool, program).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn should_ignore_a_hand_scheduled_workout() {
        // Scheduled with no program: adaptation must never touch it.
        let pool = test_pool().await;
        let (program, _, _) = program_with_one_session(&pool).await;
        let loose = workout_named(&pool, "Ad-hoc", WorkoutCategory::Endurance).await;
        schedule_workout(&pool, loose, "2026-08-06", None)
            .await
            .unwrap();

        assert_eq!(
            load_program_sessions(&pool, program).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn should_swap_the_workout_and_remember_the_original() {
        let pool = test_pool().await;
        let (program, original, entry) = program_with_one_session(&pool).await;
        let easier = workout_named(&pool, "Sweet Spot 3x12", WorkoutCategory::SweetSpot).await;

        apply_adjustment(&pool, entry, easier).await.unwrap();

        let sessions = load_program_sessions(&pool, program).await.unwrap();
        assert_eq!(sessions[0].workout_id, easier);
        assert_eq!(sessions[0].adjusted_from.as_deref(), Some("Threshold 4x8"));
        assert_ne!(sessions[0].workout_id, original);
    }

    #[tokio::test]
    async fn should_keep_pointing_at_the_first_plan_after_easing_twice() {
        let pool = test_pool().await;
        let (program, _, entry) = program_with_one_session(&pool).await;
        let sweet = workout_named(&pool, "Sweet Spot", WorkoutCategory::SweetSpot).await;
        let tempo = workout_named(&pool, "Tempo", WorkoutCategory::Tempo).await;

        apply_adjustment(&pool, entry, sweet).await.unwrap();
        apply_adjustment(&pool, entry, tempo).await.unwrap();

        let sessions = load_program_sessions(&pool, program).await.unwrap();
        assert_eq!(sessions[0].workout_id, tempo);
        assert_eq!(
            sessions[0].adjusted_from.as_deref(),
            Some("Threshold 4x8"),
            "the original plan, not the previous adjustment"
        );
    }

    #[tokio::test]
    async fn should_put_an_eased_session_back() {
        let pool = test_pool().await;
        let (program, original, entry) = program_with_one_session(&pool).await;
        let easier = workout_named(&pool, "Sweet Spot", WorkoutCategory::SweetSpot).await;

        apply_adjustment(&pool, entry, easier).await.unwrap();
        revert_adjustment(&pool, entry).await.unwrap();

        let sessions = load_program_sessions(&pool, program).await.unwrap();
        assert_eq!(sessions[0].workout_id, original);
        assert_eq!(sessions[0].adjusted_from, None);
    }

    #[tokio::test]
    async fn should_refuse_to_rewrite_a_session_already_ridden() {
        let pool = test_pool().await;
        let (program, original, entry) = program_with_one_session(&pool).await;
        sqlx::query("UPDATE calendar_entries SET completed = 1 WHERE id = ?")
            .bind(entry)
            .execute(&pool)
            .await
            .unwrap();
        let easier = workout_named(&pool, "Sweet Spot", WorkoutCategory::SweetSpot).await;

        apply_adjustment(&pool, entry, easier).await.unwrap();

        let sessions = load_program_sessions(&pool, program).await.unwrap();
        assert_eq!(
            sessions[0].workout_id, original,
            "what the rider actually rode must stand"
        );
    }

    #[tokio::test]
    async fn should_clear_only_the_future_and_unridden() {
        let pool = test_pool().await;
        let program = save_program(&pool, date(2026, 8, 3), 12, "monday")
            .await
            .unwrap();
        let w = workout_named(&pool, "Endurance", WorkoutCategory::Endurance).await;
        let past = schedule_workout(&pool, w, "2026-08-03", Some(program))
            .await
            .unwrap();
        let done = schedule_workout(&pool, w, "2026-08-12", Some(program))
            .await
            .unwrap();
        schedule_workout(&pool, w, "2026-08-14", Some(program))
            .await
            .unwrap();
        sqlx::query("UPDATE calendar_entries SET completed = 1 WHERE id = ?")
            .bind(done)
            .execute(&pool)
            .await
            .unwrap();

        let removed = clear_future_sessions(&pool, program, date(2026, 8, 10))
            .await
            .unwrap();

        assert_eq!(removed, 1, "only the future, unridden one");
        let left: Vec<i64> = load_program_sessions(&pool, program)
            .await
            .unwrap()
            .iter()
            .map(|s| s.entry_id)
            .collect();
        assert!(left.contains(&past));
        assert!(left.contains(&done));
    }

    #[tokio::test]
    async fn should_leave_another_programs_future_alone() {
        let pool = test_pool().await;
        let (mine, _, _) = program_with_one_session(&pool).await;
        let theirs = save_program(&pool, date(2026, 8, 3), 4, "monday")
            .await
            .unwrap();
        let w = workout_named(&pool, "Other", WorkoutCategory::Endurance).await;
        schedule_workout(&pool, w, "2026-08-20", Some(theirs))
            .await
            .unwrap();

        clear_future_sessions(&pool, mine, date(2026, 8, 1))
            .await
            .unwrap();

        assert_eq!(load_program_sessions(&pool, theirs).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn should_describe_the_span_of_untracked_entries() {
        let pool = test_pool().await;
        let w = workout_named(&pool, "Endurance", WorkoutCategory::Endurance).await;
        schedule_workout(&pool, w, "2026-06-16", None)
            .await
            .unwrap();
        schedule_workout(&pool, w, "2026-09-25", None)
            .await
            .unwrap();

        let (first, last, count) = orphan_entry_span(&pool)
            .await
            .unwrap()
            .expect("two loose entries");
        assert_eq!(first, date(2026, 6, 16));
        assert_eq!(last, date(2026, 9, 25));
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn should_find_no_orphans_once_everything_belongs_to_a_program() {
        let pool = test_pool().await;
        program_with_one_session(&pool).await;
        assert!(orphan_entry_span(&pool).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn should_adopt_loose_entries_including_the_ones_already_ridden() {
        let pool = test_pool().await;
        let w = workout_named(&pool, "Endurance", WorkoutCategory::Endurance).await;
        let ridden = schedule_workout(&pool, w, "2026-06-16", None)
            .await
            .unwrap();
        schedule_workout(&pool, w, "2026-09-25", None)
            .await
            .unwrap();
        sqlx::query("UPDATE calendar_entries SET completed = 1 WHERE id = ?")
            .bind(ridden)
            .execute(&pool)
            .await
            .unwrap();

        let program = save_program(&pool, date(2026, 6, 15), 15, "monday")
            .await
            .unwrap();
        let adopted = adopt_orphan_entries(&pool, program).await.unwrap();

        assert_eq!(adopted, 2, "a completed session is part of the plan too");
        let sessions = load_program_sessions(&pool, program).await.unwrap();
        assert_eq!(sessions.len(), 2);
        assert!(
            sessions.iter().any(|s| s.completed),
            "the ridden one must come back as completed, not missed"
        );
    }

    #[tokio::test]
    async fn should_not_let_a_scheduled_workout_be_deleted_from_under_the_plan() {
        // The foreign key on calendar_entries.workout_id is what guarantees
        // load_program_sessions can inner-join to workouts and always find one.
        let pool = test_pool().await;
        let (program, workout, _) = program_with_one_session(&pool).await;

        let deleted = sqlx::query("DELETE FROM workouts WHERE id = ?")
            .bind(workout)
            .execute(&pool)
            .await;

        assert!(deleted.is_err(), "a planned workout must not vanish");
        assert_eq!(
            load_program_sessions(&pool, program).await.unwrap().len(),
            1
        );
    }
}
