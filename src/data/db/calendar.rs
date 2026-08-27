//! Scheduled workouts: the plan, and what today holds.

use crate::data::workout::{Segment, Workout, WorkoutCategory};
use anyhow::Result;
use sqlx::{Row, SqlitePool};

/// What a planned day actually holds.
///
/// The schema enforces exactly one of the two (see the CHECK added in schema v4),
/// so there is no "neither" case for callers to guess at.
#[derive(Debug, Clone, PartialEq)]
pub enum ScheduledItem {
    /// A structured workout from the library.
    Workout { id: i64, name: String },
    /// A GPX route to ride.
    Route { id: i64, name: String },
}

impl ScheduledItem {
    /// What to show on the calendar for this item.
    pub fn name(&self) -> &str {
        match self {
            Self::Workout { name, .. } | Self::Route { name, .. } => name,
        }
    }

    /// The workout id, or `None` for a route.
    ///
    /// Several callers act only on workouts — completing today's session, program
    /// adaptation — and this keeps them from having to match.
    pub fn workout_id(&self) -> Option<i64> {
        match self {
            Self::Workout { id, .. } => Some(*id),
            Self::Route { .. } => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CalendarEntry {
    pub id: i64,
    /// The workout or route planned for this day.
    pub item: ScheduledItem,
    /// ISO date string "YYYY-MM-DD"
    pub scheduled_date: String,
    pub completed: bool,
    /// A route has no workout category; it is filed as endurance, which is what
    /// riding one is.
    pub category: WorkoutCategory,
    /// For a workout, the library figure. For a route, the estimate stored when
    /// it was scheduled — routes are not re-costed on every read, which would
    /// mean parsing a GPX per calendar cell.
    pub tss: f32,
    pub duration_secs: u32,
    /// The training program this entry belongs to, or `None` for anything
    /// scheduled by hand. Routes are never part of a program.
    pub program_id: Option<i64>,
    /// The name of the workout the program originally asked for, when this
    /// entry has since been eased. `None` when it still stands as planned.
    pub adjusted_from: Option<String>,
}

impl CalendarEntry {
    /// The scheduled day, or `None` if the stored text is not a date.
    ///
    /// The column is text, so a corrupted or hand-edited row can hold anything
    /// (CLAUDE.md §5.2); callers treat that as an entry with no day rather than
    /// panicking on it.
    pub fn date(&self) -> Option<chrono::NaiveDate> {
        chrono::NaiveDate::parse_from_str(&self.scheduled_date, "%Y-%m-%d").ok()
    }
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
    // Every join is LEFT: an inner join on `workouts` would silently drop every
    // scheduled route, which looks exactly like the plan losing entries, and the
    // same goes for `orig` — only an eased entry has an original to join to.
    let rows = sqlx::query(
        "SELECT ce.id, ce.workout_id, ce.route_id,
                w.name AS workout_name, r.name AS route_name,
                ce.scheduled_date, ce.completed,
                w.category, w.tss, w.duration_secs,
                ce.planned_tss, ce.planned_duration_secs,
                ce.program_id, orig.name AS original_name
         FROM calendar_entries ce
         LEFT JOIN workouts w    ON ce.workout_id          = w.id
         LEFT JOIN routes   r    ON ce.route_id            = r.id
         LEFT JOIN workouts orig ON ce.original_workout_id = orig.id
         WHERE ce.scheduled_date >= ? AND ce.scheduled_date <= ?
         ORDER BY ce.scheduled_date",
    )
    .bind(start_date)
    .bind(end_date)
    .fetch_all(pool)
    .await?;

    Ok(rows.iter().filter_map(row_to_entry).collect())
}

/// Build an entry from a joined row, or `None` if the row names nothing that
/// still exists.
///
/// The CHECK constraint guarantees a row names a workout or a route, but the
/// row it names can have been deleted from under it, leaving the join NULL. That
/// is a broken plan entry rather than a crash (CLAUDE.md §5.2), so it is logged
/// and skipped.
fn row_to_entry(r: &sqlx::sqlite::SqliteRow) -> Option<CalendarEntry> {
    let id: i64 = r.get("id");
    let workout_id: Option<i64> = r.get("workout_id");
    let route_id: Option<i64> = r.get("route_id");

    let (item, category, tss, duration_secs) = match (workout_id, route_id) {
        (Some(wid), _) => {
            let Some(name) = r.get::<Option<String>, _>("workout_name") else {
                tracing::warn!("calendar entry {id} points at missing workout {wid}; skipping");
                return None;
            };
            let category: Option<String> = r.get("category");
            (
                ScheduledItem::Workout { id: wid, name },
                WorkoutCategory::from_db_str(&category.unwrap_or_default()),
                r.get::<Option<f32>, _>("tss").unwrap_or(0.0),
                r.get::<Option<i64>, _>("duration_secs").unwrap_or(0) as u32,
            )
        }
        (None, Some(rid)) => {
            let Some(name) = r.get::<Option<String>, _>("route_name") else {
                tracing::warn!("calendar entry {id} points at missing route {rid}; skipping");
                return None;
            };
            (
                ScheduledItem::Route { id: rid, name },
                // Riding a route is an endurance day; the library's categories
                // describe interval structure a route does not have.
                WorkoutCategory::Endurance,
                r.get::<Option<f64>, _>("planned_tss").unwrap_or(0.0) as f32,
                r.get::<Option<i64>, _>("planned_duration_secs")
                    .unwrap_or(0) as u32,
            )
        }
        (None, None) => {
            tracing::warn!("calendar entry {id} names neither a workout nor a route; skipping");
            return None;
        }
    };

    Some(CalendarEntry {
        id,
        item,
        scheduled_date: r.get("scheduled_date"),
        completed: r.get::<i64, _>("completed") != 0,
        category,
        tss,
        duration_secs,
        program_id: r.get("program_id"),
        adjusted_from: r.get("original_name"),
    })
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

/// Insert a calendar entry planning a GPX route on a given ISO date.
///
/// `planned_tss` and `planned_duration_secs` come from
/// [`crate::data::route::Route::estimated_load`] and are stored rather than
/// recomputed: the estimate needs the route's points, and parsing a GPX for
/// every cell of a month grid is not something a redraw can afford.
///
/// A route is never part of a training program — programs schedule workouts they
/// can adapt — so there is no `program_id` to pass.
pub async fn schedule_route(
    pool: &SqlitePool,
    route_id: i64,
    date: &str,
    planned_tss: f32,
    planned_duration_secs: u32,
) -> Result<i64> {
    let result = sqlx::query(
        "INSERT INTO calendar_entries
             (route_id, scheduled_date, completed, planned_tss, planned_duration_secs)
         VALUES (?, ?, 0, ?, ?)",
    )
    .bind(route_id)
    .bind(date)
    .bind(planned_tss as f64)
    .bind(planned_duration_secs as i64)
    .execute(pool)
    .await?;
    Ok(result.last_insert_rowid())
}

/// Move an open calendar entry to `new_date` (ISO "YYYY-MM-DD").
///
/// Completed entries are left alone: a session that was ridden belongs to the day
/// it was ridden on, and moving it would rewrite history the load figures depend
/// on. Rescheduling one is a no-op, reported as `false`.
///
/// Returns whether a row actually moved. A mismatched id updates nothing while
/// still reporting success at the SQL level, which is indistinguishable from a
/// real save at the call site unless it is checked here.
pub async fn reschedule_entry(pool: &SqlitePool, entry_id: i64, new_date: &str) -> Result<bool> {
    let result = sqlx::query(
        "UPDATE calendar_entries SET scheduled_date = ?
         WHERE id = ? AND completed = 0",
    )
    .bind(new_date)
    .bind(entry_id)
    .execute(pool)
    .await?;

    if result.rows_affected() == 0 {
        tracing::warn!("reschedule_entry matched no open entry (id={entry_id}); nothing moved");
        return Ok(false);
    }
    Ok(true)
}

/// Mark a planned entry done, or put it back to not done.
///
/// This is the rider's own hand on the tick box, so unlike its neighbour
/// [`reschedule_entry`] it deliberately carries **no `completed = 0` guard**: it
/// has to work in both directions, and adding one for symmetry would make
/// un-marking silently do nothing.
///
/// Closing a session this way settles it against the *plan* and nothing else. It
/// banks no training load: CTL and TSB are computed from recorded sessions and
/// Intervals activities, never from calendar entries. The day's load bar does
/// move from planned to done, because that is plan accounting rather than
/// fitness.
///
/// Returns whether a row with that id was found — not whether its value changed.
/// Marking an already-done session done reports `true`, because SQLite counts
/// rows the statement visited. `false` means the entry is gone, which at a call
/// site is indistinguishable from a real save unless it is checked here.
pub async fn set_entry_completed(
    pool: &SqlitePool,
    entry_id: i64,
    completed: bool,
) -> Result<bool> {
    let result = sqlx::query("UPDATE calendar_entries SET completed = ? WHERE id = ?")
        .bind(completed)
        .bind(entry_id)
        .execute(pool)
        .await?;

    if result.rows_affected() == 0 {
        tracing::warn!("set_entry_completed matched no entry (id={entry_id}); nothing changed");
        return Ok(false);
    }
    // Logged because nothing distinguishes a hand-tick from the ride-finish tick
    // in `complete_today_calendar_entry`: un-marking a session that really was
    // ridden returns it to the missed list, where it can earn an easing it has
    // not deserved. One slightly easy session is the accepted trade, but a bug
    // report should be able to show it happened.
    if !completed {
        tracing::info!("Session marked not done by hand (id={entry_id})");
    }
    Ok(true)
}

/// A workout scheduled for a specific day, with its completion state.
pub struct TodayEntry {
    pub workout: Workout,
    pub completed: bool,
}

/// Load the first *workout* scheduled for the given ISO date, preferring
/// incomplete ones.
///
/// Scheduled routes are deliberately skipped: this feeds the dashboard's "start
/// today's session" hero, which loads a [`Workout`] into the workout player. A
/// route is ridden in the route player instead, so handing one back here would
/// give the hero something it cannot start.
pub async fn load_today_entry(pool: &SqlitePool, date: &str) -> Result<Option<TodayEntry>> {
    let row = sqlx::query(
        "SELECT ce.completed, w.id, w.name, w.description, w.duration_secs,
                w.tss, w.category, w.segments_json
         FROM calendar_entries ce
         JOIN workouts w ON ce.workout_id = w.id
         WHERE ce.scheduled_date = ? AND ce.workout_id IS NOT NULL
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::db::testing::test_pool;

    async fn a_workout(pool: &SqlitePool, name: &str) -> i64 {
        sqlx::query(
            "INSERT INTO workouts (name, duration_secs, tss, category)
             VALUES (?, 3600, 75.0, 'Endurance')",
        )
        .bind(name)
        .execute(pool)
        .await
        .unwrap()
        .last_insert_rowid()
    }

    async fn a_route(pool: &SqlitePool, name: &str) -> i64 {
        sqlx::query(
            "INSERT INTO routes (name, file_name, distance_m, elevation_gain_m, added_at)
             VALUES (?, ?, 42000, 600, '2026-01-01T00:00:00Z')",
        )
        .bind(name)
        .bind(format!("{name}.gpx"))
        .execute(pool)
        .await
        .unwrap()
        .last_insert_rowid()
    }

    async fn entries(pool: &SqlitePool) -> Vec<CalendarEntry> {
        load_calendar_entries_between(pool, "2000-01-01", "2100-01-01")
            .await
            .unwrap()
    }

    async fn a_program(pool: &SqlitePool) -> i64 {
        sqlx::query(
            "INSERT INTO programs (created_at, start_monday, num_weeks, training_days, active)
             VALUES ('2026-03-01T00:00:00Z', '2026-03-02', 8, 'Mon,Wed,Fri', 1)",
        )
        .execute(pool)
        .await
        .unwrap()
        .last_insert_rowid()
    }

    // ── the program's claim on an entry ──────────────────────────────────────

    #[tokio::test]
    async fn should_report_which_program_owns_an_entry() {
        let pool = test_pool().await;
        let p = a_program(&pool).await;
        let owned = a_workout(&pool, "Threshold 2x20").await;
        let hand = a_workout(&pool, "Coffee Ride").await;
        schedule_workout(&pool, owned, "2026-03-04", Some(p))
            .await
            .unwrap();
        schedule_workout(&pool, hand, "2026-03-05", None)
            .await
            .unwrap();

        let found = entries(&pool).await;
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].program_id, Some(p));
        assert_eq!(found[1].program_id, None);
    }

    #[tokio::test]
    async fn should_report_a_scheduled_route_as_owned_by_no_program() {
        // `schedule_route` takes no program id at all — a route is never part
        // of a program, and the calendar relies on that to skip marking them.
        let pool = test_pool().await;
        let r = a_route(&pool, "Alpe d'Huez").await;
        schedule_route(&pool, r, "2026-03-05", 118.0, 7200)
            .await
            .unwrap();

        let found = entries(&pool).await;
        assert_eq!(found[0].program_id, None);
        assert_eq!(found[0].adjusted_from, None);
    }

    #[tokio::test]
    async fn should_name_the_workout_an_eased_entry_came_from() {
        let pool = test_pool().await;
        let p = a_program(&pool).await;
        let hard = a_workout(&pool, "Threshold 2x20").await;
        let easy = a_workout(&pool, "Sweet Spot 3x12").await;
        let id = schedule_workout(&pool, hard, "2026-03-04", Some(p))
            .await
            .unwrap();

        let before = entries(&pool).await;
        assert_eq!(before[0].adjusted_from, None);

        crate::data::db::apply_adjustment(&pool, id, easy)
            .await
            .unwrap();

        let after = entries(&pool).await;
        assert_eq!(after[0].item.name(), "Sweet Spot 3x12");
        assert_eq!(after[0].adjusted_from.as_deref(), Some("Threshold 2x20"));
    }

    #[tokio::test]
    async fn should_keep_returning_entries_that_were_never_eased() {
        // Guards the third LEFT JOIN the same way the route test guards the
        // first two: an inner join on `orig` would return only eased entries,
        // silently emptying the calendar for everyone else.
        let pool = test_pool().await;
        let p = a_program(&pool).await;
        let w = a_workout(&pool, "Endurance 90").await;
        let r = a_route(&pool, "Ventoux").await;
        schedule_workout(&pool, w, "2026-03-04", Some(p))
            .await
            .unwrap();
        schedule_route(&pool, r, "2026-03-05", 118.0, 7200)
            .await
            .unwrap();

        let found = entries(&pool).await;
        assert_eq!(found.len(), 2, "no entry may be dropped by the orig join");
        assert!(found.iter().all(|e| e.adjusted_from.is_none()));
    }

    // ── routes on the calendar ───────────────────────────────────────────────

    #[tokio::test]
    async fn should_return_scheduled_routes_alongside_workouts() {
        // Guards the LEFT JOIN. An inner join on `workouts` drops every route
        // row, which reads as the plan quietly losing entries rather than as a
        // query bug.
        let pool = test_pool().await;
        let w = a_workout(&pool, "Threshold 60").await;
        let r = a_route(&pool, "Col de Sarenne").await;
        schedule_workout(&pool, w, "2026-03-04", None)
            .await
            .unwrap();
        schedule_route(&pool, r, "2026-03-05", 118.0, 7200)
            .await
            .unwrap();

        let found = entries(&pool).await;
        assert_eq!(found.len(), 2, "both the workout and the route must appear");
        assert_eq!(
            found[0].item,
            ScheduledItem::Workout {
                id: w,
                name: "Threshold 60".into()
            }
        );
        assert_eq!(
            found[1].item,
            ScheduledItem::Route {
                id: r,
                name: "Col de Sarenne".into()
            }
        );
    }

    #[tokio::test]
    async fn should_report_a_routes_stored_estimate_as_its_load() {
        // A route carries the estimate made when it was scheduled; nothing
        // re-parses the GPX to redraw a calendar.
        let pool = test_pool().await;
        let r = a_route(&pool, "Alpe").await;
        schedule_route(&pool, r, "2026-03-05", 118.5, 7200)
            .await
            .unwrap();

        let found = entries(&pool).await;
        assert!((found[0].tss - 118.5).abs() < 0.01);
        assert_eq!(found[0].duration_secs, 7200);
    }

    #[tokio::test]
    async fn should_not_offer_a_route_as_todays_workout() {
        // The dashboard hero loads a Workout into the workout player; a route
        // cannot be started there.
        let pool = test_pool().await;
        let r = a_route(&pool, "Alpe").await;
        schedule_route(&pool, r, "2026-03-05", 118.0, 7200)
            .await
            .unwrap();

        assert!(load_today_entry(&pool, "2026-03-05")
            .await
            .unwrap()
            .is_none());
    }

    // ── rescheduling ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn should_move_an_open_entry_to_another_day() {
        let pool = test_pool().await;
        let w = a_workout(&pool, "Threshold 60").await;
        let id = schedule_workout(&pool, w, "2026-03-04", None)
            .await
            .unwrap();

        assert!(reschedule_entry(&pool, id, "2026-03-09").await.unwrap());

        let found = entries(&pool).await;
        assert_eq!(found.len(), 1, "rescheduling moves, it does not duplicate");
        assert_eq!(found[0].scheduled_date, "2026-03-09");
    }

    #[tokio::test]
    async fn should_move_a_scheduled_route_too() {
        let pool = test_pool().await;
        let r = a_route(&pool, "Alpe").await;
        let id = schedule_route(&pool, r, "2026-03-05", 118.0, 7200)
            .await
            .unwrap();

        assert!(reschedule_entry(&pool, id, "2026-03-12").await.unwrap());
        assert_eq!(entries(&pool).await[0].scheduled_date, "2026-03-12");
    }

    #[tokio::test]
    async fn should_refuse_to_move_a_completed_session() {
        // A ridden session belongs to the day it was ridden on; the load figures
        // for that week are computed from it.
        let pool = test_pool().await;
        let w = a_workout(&pool, "Threshold 60").await;
        let id = schedule_workout(&pool, w, "2026-03-04", None)
            .await
            .unwrap();
        complete_today_calendar_entry(&pool, w, "2026-03-04")
            .await
            .unwrap();

        assert!(
            !reschedule_entry(&pool, id, "2026-03-09").await.unwrap(),
            "a completed entry must not move"
        );
        assert_eq!(entries(&pool).await[0].scheduled_date, "2026-03-04");
    }

    #[tokio::test]
    async fn should_report_that_nothing_moved_for_an_unknown_entry() {
        // An UPDATE matching no row still succeeds at the SQL level. Without this
        // the caller cannot tell a save from a silent no-op.
        let pool = test_pool().await;
        assert!(!reschedule_entry(&pool, 9999, "2026-03-09").await.unwrap());
    }
    // ── the rider's own hand on the tick box ─────────────────────────────────

    #[tokio::test]
    async fn should_mark_a_planned_session_done() {
        let pool = test_pool().await;
        let w = a_workout(&pool, "Tempo 20").await;
        let id = schedule_workout(&pool, w, "2026-03-04", None)
            .await
            .unwrap();

        assert!(set_entry_completed(&pool, id, true).await.unwrap());
        assert!(entries(&pool).await[0].completed);
    }

    #[tokio::test]
    async fn should_put_a_session_back_to_not_done() {
        // The guard test. `set_entry_completed` deliberately has no
        // `AND completed = 0` clause, unlike `reschedule_entry` next to it.
        // Adding one for symmetry would break exactly this, and silently.
        let pool = test_pool().await;
        let w = a_workout(&pool, "Tempo 20").await;
        let id = schedule_workout(&pool, w, "2026-03-04", None)
            .await
            .unwrap();
        set_entry_completed(&pool, id, true).await.unwrap();

        assert!(set_entry_completed(&pool, id, false).await.unwrap());
        assert!(!entries(&pool).await[0].completed);
    }

    #[tokio::test]
    async fn should_report_that_nothing_changed_when_marking_an_unknown_entry() {
        let pool = test_pool().await;
        assert!(!set_entry_completed(&pool, 9999, true).await.unwrap());
    }

    #[tokio::test]
    async fn should_report_success_when_marking_an_already_done_session_done() {
        // SQLite counts the rows an UPDATE visited, not the ones whose value
        // actually changed, so this is `true`. Pinned so it is not "optimised"
        // into `false` on the assumption that it reports a state change.
        let pool = test_pool().await;
        let w = a_workout(&pool, "Tempo 20").await;
        let id = schedule_workout(&pool, w, "2026-03-04", None)
            .await
            .unwrap();
        set_entry_completed(&pool, id, true).await.unwrap();

        assert!(set_entry_completed(&pool, id, true).await.unwrap());
        assert!(entries(&pool).await[0].completed);
    }

    #[tokio::test]
    async fn should_still_refuse_to_move_a_session_marked_done_by_hand() {
        // The two guards compose: ticking a session by hand makes it as immovable
        // as riding it does.
        let pool = test_pool().await;
        let w = a_workout(&pool, "Tempo 20").await;
        let id = schedule_workout(&pool, w, "2026-03-04", None)
            .await
            .unwrap();
        set_entry_completed(&pool, id, true).await.unwrap();

        assert!(!reschedule_entry(&pool, id, "2026-03-09").await.unwrap());
        assert_eq!(entries(&pool).await[0].scheduled_date, "2026-03-04");
    }
}
