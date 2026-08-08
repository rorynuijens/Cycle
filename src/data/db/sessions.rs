//! Recorded rides: writing them, checkpointing one in progress, and the Intervals.icu links that stop a ride being counted twice.

use anyhow::Result;

// Reconciling a ride against Intervals.icu reads the mirror, and the one-shot
// backfill records that it has run in the settings table.
use super::{get_setting, load_intervals_activities, set_setting, IntervalsActivity};
use crate::data::session::{DataPoint, Session};
use chrono::{DateTime, NaiveDate, Utc};
use sqlx::{Row, SqlitePool};

/// Persist a completed session and return its new DB id.
/// A ride reduced to the figures the list views actually need, read straight
/// from columns without touching `data_points_json`.
///
/// A recorded second costs roughly 140–210 bytes of JSON, so an hour of riding
/// is a half-megabyte blob. Deserialising every ride ever recorded on every page
/// navigation is what this type exists to avoid; reach for
/// [`load_session_records`] only when the samples themselves are needed.
#[derive(Debug, Clone)]
pub struct SessionSummary {
    /// Identifies the ride so callers can go on to load or act on it.
    #[allow(dead_code)]
    pub id: i64,
    pub started_at: DateTime<Utc>,
    pub duration_secs: u64,
    pub normalised_power: Option<f32>,
    pub average_power: Option<f32>,
    pub kilojoules: f32,
    /// FTP at ride time; `None` for rides recorded before it was stamped.
    pub ftp_watts: Option<u32>,
    pub rpe: Option<u8>,
    /// Name to file the ride under: its own title, else the workout's.
    pub workout_name: Option<String>,
    pub uploaded_to_icu: bool,
    pub icu_id: Option<String>,
}

impl SessionSummary {
    /// Training Stress Score, scored against the FTP the ride was ridden at.
    ///
    /// `fallback_ftp` is used only when the ride carries no stamped FTP, matching
    /// [`Session::tss`].
    pub fn tss(&self, fallback_ftp: u32) -> Option<f32> {
        let ftp = self.ftp_watts.unwrap_or(fallback_ftp);
        if ftp == 0 {
            return None;
        }
        let np = self.normalised_power?;
        let intensity = np / ftp as f32;
        let hours = self.duration_secs as f32 / 3600.0;
        Some(intensity.powi(2) * hours * 100.0)
    }

    /// Is this ride already represented in `intervals_activities`?
    ///
    /// True whether the app uploaded it directly or it came back from
    /// Intervals.icu after a round trip through Garmin or Strava and was matched
    /// to it. Load calculations must skip such rides, since the same training
    /// stress arrives again through the Intervals.icu figures.
    pub fn counted_via_intervals(&self) -> bool {
        self.uploaded_to_icu || self.icu_id.is_some()
    }
}

impl SessionRecord {
    /// Reduce a fully-loaded ride to its summary, so code that has records can
    /// share the paths written against [`SessionSummary`].
    pub fn summary(&self) -> SessionSummary {
        SessionSummary {
            id: self.session.id,
            started_at: self.session.started_at,
            duration_secs: self.session.duration_secs(),
            normalised_power: self.session.normalised_power(),
            average_power: self.session.average_power(),
            kilojoules: self.session.kilojoules(),
            ftp_watts: self.session.ftp_watts,
            rpe: self.session.rpe,
            workout_name: self.workout_name.clone(),
            uploaded_to_icu: self.uploaded_to_icu,
            icu_id: self.session.icu_id.clone(),
        }
    }
}

/// Every completed ride, as summaries. Does not read `data_points_json`.
pub async fn load_session_summaries(pool: &SqlitePool) -> Result<Vec<SessionSummary>> {
    let rows = sqlx::query(
        "SELECT s.id, s.started_at, s.duration_secs, s.normalised_power,
                s.average_power, s.kilojoules, s.ftp_watts, s.rpe, s.icu_id,
                s.uploaded_to_icu, COALESCE(s.title, w.name) AS workout_name
         FROM sessions s
         LEFT JOIN workouts w ON s.workout_id = w.id
         WHERE s.ended_at IS NOT NULL
         ORDER BY s.started_at DESC",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| SessionSummary {
            id: r.get("id"),
            started_at: DateTime::parse_from_rfc3339(r.get::<&str, _>("started_at"))
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
            duration_secs: r.get::<Option<i64>, _>("duration_secs").unwrap_or(0).max(0) as u64,
            normalised_power: r
                .get::<Option<f64>, _>("normalised_power")
                .map(|v| v as f32),
            average_power: r.get::<Option<f64>, _>("average_power").map(|v| v as f32),
            kilojoules: r.get::<Option<f64>, _>("kilojoules").unwrap_or(0.0) as f32,
            ftp_watts: r.get::<Option<i64>, _>("ftp_watts").map(|v| v as u32),
            rpe: r.get::<Option<i64>, _>("rpe").map(|v| v as u8),
            workout_name: r.get("workout_name"),
            uploaded_to_icu: r.get::<i64, _>("uploaded_to_icu") != 0,
            icu_id: r.get("icu_id"),
        })
        .collect())
}

/// Fill in the metric columns for rides written before they existed. Reads the
/// blobs once, at migration, so no later read has to.
pub(super) async fn backfill_session_metrics(pool: &SqlitePool) -> Result<()> {
    let rows = sqlx::query(
        "SELECT id, started_at, ended_at, data_points_json
           FROM sessions
          WHERE duration_secs IS NULL AND ended_at IS NOT NULL",
    )
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        return Ok(());
    }

    tracing::info!("Backfilling ride metrics for {} sessions", rows.len());
    for r in rows {
        let id: i64 = r.get("id");
        let data_points: Vec<DataPoint> = match serde_json::from_str(r.get("data_points_json")) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("session {id}: cannot backfill metrics ({e})");
                continue;
            }
        };
        let mut session = Session::new(None);
        session.started_at = DateTime::parse_from_rfc3339(r.get::<&str, _>("started_at"))
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());
        session.ended_at = r
            .get::<Option<&str>, _>("ended_at")
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        session.data_points = data_points;
        write_session_metrics(pool, id, &session).await?;
    }
    Ok(())
}

/// Recompute and store the metric columns for one ride.
async fn write_session_metrics(pool: &SqlitePool, id: i64, session: &Session) -> Result<()> {
    sqlx::query(
        "UPDATE sessions
            SET duration_secs = ?, normalised_power = ?, average_power = ?, kilojoules = ?
          WHERE id = ?",
    )
    .bind(session.duration_secs() as i64)
    .bind(session.normalised_power().map(|v| v as f64))
    .bind(session.average_power().map(|v| v as f64))
    .bind(session.kilojoules() as f64)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn save_session(pool: &SqlitePool, session: &Session) -> Result<i64> {
    upsert_session(pool, None, session).await
}

/// Write a session, inserting a new row or overwriting an existing one.
///
/// `id` is `None` for a ride that has never been written and `Some` for one that
/// is already on disk — which is how mid-ride checkpoints keep updating a single
/// row rather than accumulating a row per checkpoint. A row whose `ended_at` is
/// NULL is a ride that was interrupted; see [`load_unfinished_sessions`].
pub async fn upsert_session(pool: &SqlitePool, id: Option<i64>, session: &Session) -> Result<i64> {
    let data_points_json = serde_json::to_string(&session.data_points)?;
    let started_at = session.started_at.to_rfc3339();
    let ended_at = session.ended_at.map(|t| t.to_rfc3339());

    let row_id = match id {
        Some(existing) => {
            sqlx::query(
                "UPDATE sessions
                    SET workout_id = ?, started_at = ?, ended_at = ?, data_points_json = ?,
                        ftp_watts = ?, title = ?, icu_id = ?
                  WHERE id = ?",
            )
            .bind(session.workout_id)
            .bind(&started_at)
            .bind(&ended_at)
            .bind(&data_points_json)
            .bind(session.ftp_watts.map(|v| v as i64))
            .bind(session.title.as_deref())
            .bind(session.icu_id.as_deref())
            .bind(existing)
            .execute(pool)
            .await?;
            existing
        }
        None => {
            let result = sqlx::query(
                "INSERT INTO sessions (workout_id, started_at, ended_at, data_points_json,
                                       ftp_watts, title, icu_id)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(session.workout_id)
            .bind(&started_at)
            .bind(&ended_at)
            .bind(&data_points_json)
            .bind(session.ftp_watts.map(|v| v as i64))
            .bind(session.title.as_deref())
            .bind(session.icu_id.as_deref())
            .execute(pool)
            .await?;
            result.last_insert_rowid()
        }
    };

    // Derive the list-view metrics once, here, so no read path has to reopen the
    // blob to get them back.
    write_session_metrics(pool, row_id, session).await?;
    Ok(row_id)
}

/// Write a mid-ride checkpoint, inserting the row on the first call and updating
/// it thereafter. Returns the row id.
///
/// Unlike [`upsert_session`] this never writes `ended_at`, and its UPDATE matches
/// only rows that are still unfinished. Both matter because a checkpoint issued
/// just before the rider finishes can land *after* the finishing write: the
/// `ended_at IS NULL` guard turns that late checkpoint into a no-op instead of
/// letting it reopen a completed ride or truncate its final seconds.
pub async fn checkpoint_session(
    pool: &SqlitePool,
    id: Option<i64>,
    session: &Session,
) -> Result<i64> {
    let data_points_json = serde_json::to_string(&session.data_points)?;
    let started_at = session.started_at.to_rfc3339();

    match id {
        Some(existing) => {
            sqlx::query(
                "UPDATE sessions
                    SET workout_id = ?, started_at = ?, data_points_json = ?,
                        ftp_watts = ?, title = ?
                  WHERE id = ? AND ended_at IS NULL",
            )
            .bind(session.workout_id)
            .bind(&started_at)
            .bind(&data_points_json)
            .bind(session.ftp_watts.map(|v| v as i64))
            .bind(session.title.as_deref())
            .bind(existing)
            .execute(pool)
            .await?;
            Ok(existing)
        }
        None => {
            let result = sqlx::query(
                "INSERT INTO sessions (workout_id, started_at, ended_at, data_points_json,
                                       ftp_watts, title)
                 VALUES (?, ?, NULL, ?, ?, ?)",
            )
            .bind(session.workout_id)
            .bind(&started_at)
            .bind(&data_points_json)
            .bind(session.ftp_watts.map(|v| v as i64))
            .bind(session.title.as_deref())
            .execute(pool)
            .await?;
            Ok(result.last_insert_rowid())
        }
    }
}

/// Rides that were checkpointed but never finished — the app was closed, crashed
/// or lost power mid-session. Offered back to the rider at startup.
pub async fn load_unfinished_sessions(pool: &SqlitePool) -> Result<Vec<SessionRecord>> {
    load_session_records_where(pool, "s.ended_at IS NULL").await
}

/// Close out a recovered ride, stamping the end time the rider actually stopped at.
pub async fn finalise_session(pool: &SqlitePool, id: i64, ended_at: &str) -> Result<()> {
    sqlx::query("UPDATE sessions SET ended_at = ? WHERE id = ?")
        .bind(ended_at)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Link a session to the Intervals.icu activity that is the same ride, or pass
/// `None` to unlink it.
pub async fn set_session_icu_id(
    pool: &SqlitePool,
    session_id: i64,
    icu_id: Option<&str>,
) -> Result<()> {
    sqlx::query("UPDATE sessions SET icu_id = ? WHERE id = ?")
        .bind(icu_id)
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Unlink a ride from its Intervals.icu activity at the rider's request, and
/// remember the decision so the matcher does not pair them again.
pub async fn unlink_session_from_icu(pool: &SqlitePool, session_id: i64) -> Result<()> {
    sqlx::query("UPDATE sessions SET icu_id = NULL, icu_link_rejected = 1 WHERE id = ?")
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Match unlinked local sessions against synced Intervals.icu activities and record
/// the links found. Returns how many new links were made.
///
/// Runs after an Intervals.icu sync and after a session is saved, because either
/// record can arrive first: a ride recorded today reaches Intervals.icu hours later
/// via Garmin, while a FIT file imported from an old ride may arrive long after
/// Intervals.icu already knew about it.
///
/// Only the recent past is considered — re-examining years of history on every sync
/// would be wasted work, and a ride old enough to have fallen out of this window has
/// long since been matched or will never match.
pub async fn reconcile_icu_links(pool: &SqlitePool) -> Result<usize> {
    use crate::data::dedupe;

    const RECONCILE_WINDOW_DAYS: i64 = 60;

    let cutoff = (Utc::now() - chrono::Duration::days(RECONCILE_WINDOW_DAYS)).to_rfc3339();

    // Rides the rider has explicitly unlinked must not be paired up again.
    let rejected: std::collections::HashSet<i64> =
        sqlx::query("SELECT id FROM sessions WHERE icu_link_rejected = 1")
            .fetch_all(pool)
            .await?
            .into_iter()
            .map(|r| r.get::<i64, _>("id"))
            .collect();

    let sessions: Vec<SessionRecord> = load_session_records(pool)
        .await?
        .into_iter()
        .filter(|r| {
            r.session.icu_id.is_none()
                && !rejected.contains(&r.session.id)
                && r.session.started_at.to_rfc3339() >= cutoff
        })
        .collect();
    if sessions.is_empty() {
        return Ok(0);
    }

    // An activity already claimed by another session must not be claimed twice.
    let mut taken: std::collections::HashSet<String> =
        sqlx::query("SELECT icu_id FROM sessions WHERE icu_id IS NOT NULL")
            .fetch_all(pool)
            .await?
            .into_iter()
            .map(|r| r.get::<String, _>("icu_id"))
            .collect();

    let activities = load_intervals_activities(pool).await?;
    let mut linked = 0;

    for record in sessions {
        let candidates: Vec<&IntervalsActivity> = activities
            .iter()
            .filter(|a| !taken.contains(&a.icu_id))
            .collect();
        if let Some(activity) = dedupe::find_match(&record.session, candidates) {
            set_session_icu_id(pool, record.session.id, Some(&activity.icu_id)).await?;
            taken.insert(activity.icu_id.clone());
            linked += 1;
            tracing::info!(
                "Linked session {} to Intervals.icu activity {}",
                record.session.id,
                activity.icu_id
            );
        }
    }

    Ok(linked)
}

/// Setting key marking the one-off legacy backfill as done.
const LEGACY_BACKFILL_KEY: &str = "dedupe.legacy_backfill_done";

/// One-off pass linking rides recorded before the start-time fix.
///
/// Those sessions were stamped when the workout was selected rather than when the
/// rider started pedalling, so their start times sit minutes early and the everyday
/// matcher's three-minute window misses them — leaving historic duplicates on the
/// calendar. This runs once over the whole history using
/// [`dedupe::is_same_activity_legacy`], which asks the real ride to fit inside the
/// session's inflated span instead, then records that it has run.
///
/// **Temporary.** Once it has run on every installation this function, its setting
/// key and `dedupe::is_same_activity_legacy` can all be deleted; nothing else
/// depends on them. Any link it gets wrong is reversible with Unlink in the ride's
/// detail dialog.
pub async fn backfill_icu_links(pool: &SqlitePool) -> Result<usize> {
    use crate::data::dedupe;

    if get_setting(pool, LEGACY_BACKFILL_KEY).await?.is_some() {
        return Ok(0);
    }

    let rejected: std::collections::HashSet<i64> =
        sqlx::query("SELECT id FROM sessions WHERE icu_link_rejected = 1")
            .fetch_all(pool)
            .await?
            .into_iter()
            .map(|r| r.get::<i64, _>("id"))
            .collect();

    let mut taken = linked_icu_ids(pool).await?;
    let activities = load_intervals_activities(pool).await?;
    let mut linked = 0;

    for record in load_session_records(pool).await? {
        if record.session.icu_id.is_some() || rejected.contains(&record.session.id) {
            continue;
        }
        let candidates: Vec<&IntervalsActivity> = activities
            .iter()
            .filter(|a| !taken.contains(&a.icu_id))
            .collect();
        if let Some(activity) =
            dedupe::find_match_with(&record.session, candidates, dedupe::is_same_activity_legacy)
        {
            set_session_icu_id(pool, record.session.id, Some(&activity.icu_id)).await?;
            taken.insert(activity.icu_id.clone());
            linked += 1;
            tracing::info!(
                "Backfill linked session {} to Intervals.icu activity {}",
                record.session.id,
                activity.icu_id
            );
        }
    }

    set_setting(pool, LEGACY_BACKFILL_KEY, "1").await?;
    tracing::info!("Legacy Intervals.icu backfill linked {linked} historic ride(s)");
    Ok(linked)
}

/// The Intervals.icu activities already accounted for by a local session. Callers
/// use this to show and count each ride once.
pub async fn linked_icu_ids(pool: &SqlitePool) -> Result<std::collections::HashSet<String>> {
    let rows = sqlx::query("SELECT icu_id FROM sessions WHERE icu_id IS NOT NULL")
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| r.get::<String, _>("icu_id"))
        .collect())
}

/// Rename a saved activity. An empty `title` clears the name, so the session
/// falls back to its linked workout's name (or "Unstructured Ride").
pub async fn set_session_title(pool: &SqlitePool, session_id: i64, title: &str) -> Result<()> {
    let trimmed = title.trim();
    let value = (!trimmed.is_empty()).then_some(trimmed);
    sqlx::query("UPDATE sessions SET title = ? WHERE id = ?")
        .bind(value)
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Update the RPE score on an already-saved session.
pub async fn save_session_rpe(pool: &SqlitePool, session_id: i64, rpe: u8) -> Result<()> {
    sqlx::query("UPDATE sessions SET rpe = ? WHERE id = ?")
        .bind(rpe as i64)
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete_session(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// A stored session row, joined with its workout name.
#[derive(Clone)]
pub struct SessionRecord {
    pub session: Session,
    pub workout_name: Option<String>,
    /// True when this session was successfully uploaded to Intervals.icu.
    /// Such sessions are excluded from local CTL calculation; they are already
    /// counted via `intervals_activities`.
    pub uploaded_to_icu: bool,
}

/// Returns true if any session already has the given `started_at` timestamp — used
/// to prevent re-importing the same FIT file twice.
pub async fn session_exists_at(pool: &SqlitePool, started_at: &DateTime<Utc>) -> Result<bool> {
    let ts = started_at.to_rfc3339();
    let row = sqlx::query("SELECT 1 FROM sessions WHERE started_at = ? LIMIT 1")
        .bind(&ts)
        .fetch_optional(pool)
        .await?;
    Ok(row.is_some())
}

/// Load all sessions, newest first, joined with workout names.
/// Every completed ride. Rides still in progress (checkpointed but not finished)
/// are excluded — they must not reach history, the fitness curve or the calendar
/// until the rider has actually finished or recovered them.
pub async fn load_session_records(pool: &SqlitePool) -> Result<Vec<SessionRecord>> {
    load_session_records_where(pool, "s.ended_at IS NOT NULL").await
}

/// Shared body of the full-fidelity session loaders. `predicate` is a trusted
/// literal supplied by this module only — never user input, so it is inlined
/// rather than bound (there is no way to parameterise a WHERE clause in SQL).
async fn load_session_records_where(
    pool: &SqlitePool,
    predicate: &'static str,
) -> Result<Vec<SessionRecord>> {
    let rows = sqlx::query(&format!(
        "SELECT s.id, s.workout_id, s.started_at, s.ended_at, s.data_points_json,
                s.uploaded_to_icu, s.rpe, s.ftp_watts, s.title, s.icu_id,
                COALESCE(s.title, w.name) AS workout_name
         FROM sessions s
         LEFT JOIN workouts w ON s.workout_id = w.id
         WHERE {predicate}
         ORDER BY s.started_at DESC"
    ))
    .fetch_all(pool)
    .await?;

    let mut records = Vec::new();
    for r in rows {
        let session_id: i64 = r.get("id");
        // A blob that will not parse means the ride's samples are gone. Keep the
        // row so the ride is still listed, but say so — silently substituting an
        // empty ride makes it show 0 TSS and vanish from the fitness curve with
        // no indication anything was lost.
        let data_points: Vec<DataPoint> = match serde_json::from_str(r.get("data_points_json")) {
            Ok(points) => points,
            Err(e) => {
                tracing::error!(
                    "session {session_id}: data points unreadable ({e}) — treating as empty"
                );
                Vec::new()
            }
        };

        let started_at: DateTime<Utc> =
            DateTime::parse_from_rfc3339(r.get::<&str, _>("started_at"))
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());

        let ended_at: Option<DateTime<Utc>> = r
            .get::<Option<&str>, _>("ended_at")
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));

        records.push(SessionRecord {
            session: Session {
                id: session_id,
                workout_id: r.get("workout_id"),
                started_at,
                ended_at,
                data_points,
                rpe: r.get::<Option<i64>, _>("rpe").map(|v| v as u8),
                ftp_watts: r.get::<Option<i64>, _>("ftp_watts").map(|v| v as u32),
                title: r.get("title"),
                icu_id: r.get("icu_id"),
            },
            workout_name: r.get("workout_name"),
            uploaded_to_icu: r.get::<i64, _>("uploaded_to_icu") != 0,
        });
    }
    Ok(records)
}

/// Mark a session as successfully uploaded to Intervals.icu so its TSS is not
/// double-counted alongside the same activity in `intervals_activities`.
pub async fn mark_session_uploaded_to_icu(pool: &SqlitePool, session_id: i64) -> Result<()> {
    sqlx::query("UPDATE sessions SET uploaded_to_icu = 1 WHERE id = ?")
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Load sessions whose start time falls within [start_utc, end_utc] (ISO 8601), newest first.
pub async fn load_sessions_between(
    pool: &SqlitePool,
    start_utc: &str,
    end_utc: &str,
) -> Result<Vec<SessionRecord>> {
    let rows = sqlx::query(
        "SELECT s.id, s.workout_id, s.started_at, s.ended_at, s.data_points_json,
                s.uploaded_to_icu, s.rpe, s.ftp_watts, s.title, s.icu_id,
                COALESCE(s.title, w.name) AS workout_name
         FROM sessions s
         LEFT JOIN workouts w ON s.workout_id = w.id
         WHERE s.started_at >= ? AND s.started_at <= ? AND s.ended_at IS NOT NULL
         ORDER BY s.started_at ASC",
    )
    .bind(start_utc)
    .bind(end_utc)
    .fetch_all(pool)
    .await?;

    let mut records = Vec::new();
    for r in rows {
        let data_points: Vec<DataPoint> =
            serde_json::from_str(r.get("data_points_json")).unwrap_or_default();
        let started_at: DateTime<Utc> =
            DateTime::parse_from_rfc3339(r.get::<&str, _>("started_at"))
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
        let ended_at: Option<DateTime<Utc>> = r
            .get::<Option<&str>, _>("ended_at")
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        records.push(SessionRecord {
            session: Session {
                id: r.get("id"),
                workout_id: r.get("workout_id"),
                started_at,
                ended_at,
                data_points,
                rpe: r.get::<Option<i64>, _>("rpe").map(|v| v as u8),
                ftp_watts: r.get::<Option<i64>, _>("ftp_watts").map(|v| v as u32),
                title: r.get("title"),
                icu_id: r.get("icu_id"),
            },
            workout_name: r.get("workout_name"),
            uploaded_to_icu: r.get::<i64, _>("uploaded_to_icu") != 0,
        });
    }
    Ok(records)
}

/// Load past in-app sessions whose start date (local) falls within [start_date, end_date].
pub async fn load_sessions_for_dates(
    pool: &SqlitePool,
    start_date: NaiveDate,
    end_date: NaiveDate,
) -> Result<Vec<SessionRecord>> {
    use chrono::{Local, TimeZone};
    let start_utc = Local
        .from_local_datetime(&start_date.and_hms_opt(0, 0, 0).expect("valid time"))
        .earliest()
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .unwrap_or_else(chrono::Utc::now);
    let end_utc = Local
        .from_local_datetime(&end_date.and_hms_opt(23, 59, 59).expect("valid time"))
        .latest()
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .unwrap_or_else(chrono::Utc::now);
    load_sessions_between(pool, &start_utc.to_rfc3339(), &end_utc.to_rfc3339()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    // Rides are checked against the mirror and the route list, so these tests
    // reach the sibling modules through the parent's re-exports.
    use crate::data::db::testing::*;
    use crate::data::db::*;

    #[tokio::test]
    async fn save_session_persists_data_points() {
        let pool = test_pool().await;
        let mut session = Session::new(None);
        session.data_points.push(DataPoint {
            elapsed_secs: 0,
            power_watts: Some(210),
            target_watts: None,
            heart_rate_bpm: Some(150),
            cadence_rpm: Some(90),
            speed_kmh: Some(32.0),
            lat: None,
            lng: None,
            altitude_m: None,
        });
        let id = save_session(&pool, &session).await.unwrap();
        assert!(id > 0);

        let json: String = sqlx::query("SELECT data_points_json FROM sessions WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("data_points_json");
        let points: Vec<DataPoint> = serde_json::from_str(&json).unwrap();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].power_watts, Some(210));
    }

    #[tokio::test]
    async fn save_session_persists_target_and_ftp() {
        let pool = test_pool().await;
        let mut session = Session::new(None);
        session.ftp_watts = Some(250);
        session.data_points.push(DataPoint {
            elapsed_secs: 0,
            power_watts: Some(228),
            target_watts: Some(230),
            heart_rate_bpm: None,
            cadence_rpm: None,
            speed_kmh: None,
            lat: None,
            lng: None,
            altitude_m: None,
        });
        let id = save_session(&pool, &session).await.unwrap();

        let row = sqlx::query("SELECT ftp_watts, data_points_json FROM sessions WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.get::<Option<i64>, _>("ftp_watts"), Some(250));
        let points: Vec<DataPoint> =
            serde_json::from_str(row.get::<&str, _>("data_points_json")).unwrap();
        assert_eq!(points[0].target_watts, Some(230));
    }

    fn riding_session(secs: u32) -> Session {
        let mut s = Session::new(None);
        s.ftp_watts = Some(250);
        for i in 0..secs {
            s.data_points.push(DataPoint {
                elapsed_secs: i,
                power_watts: Some(200),
                target_watts: None,
                heart_rate_bpm: None,
                cadence_rpm: None,
                speed_kmh: None,
                lat: None,
                lng: None,
                altitude_m: None,
            });
        }
        s
    }

    #[tokio::test]
    async fn checkpoints_keep_updating_one_row() {
        let pool = test_pool().await;
        let first = checkpoint_session(&pool, None, &riding_session(30))
            .await
            .unwrap();
        let second = checkpoint_session(&pool, Some(first), &riding_session(60))
            .await
            .unwrap();
        assert_eq!(first, second, "a checkpoint must reuse its row");

        let unfinished = load_unfinished_sessions(&pool).await.unwrap();
        assert_eq!(unfinished.len(), 1);
        assert_eq!(unfinished[0].session.data_points.len(), 60);
    }

    #[tokio::test]
    async fn an_unfinished_ride_stays_out_of_history() {
        let pool = test_pool().await;
        checkpoint_session(&pool, None, &riding_session(30))
            .await
            .unwrap();
        assert!(
            load_session_records(&pool).await.unwrap().is_empty(),
            "a ride still in progress must not appear in history"
        );
    }

    #[tokio::test]
    async fn finishing_overwrites_the_checkpoint_instead_of_adding_a_row() {
        let pool = test_pool().await;
        let row = checkpoint_session(&pool, None, &riding_session(30))
            .await
            .unwrap();

        let mut finished = riding_session(45);
        finished.ended_at = Some(Utc::now());
        upsert_session(&pool, Some(row), &finished).await.unwrap();

        let records = load_session_records(&pool).await.unwrap();
        assert_eq!(records.len(), 1, "finishing must not insert a second ride");
        assert_eq!(records[0].session.data_points.len(), 45);
        assert!(load_unfinished_sessions(&pool).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_late_checkpoint_cannot_reopen_a_finished_ride() {
        // The checkpoint issued just before the rider finishes can land after the
        // finishing write. It must not clear ended_at or roll back the last
        // seconds of the ride.
        let pool = test_pool().await;
        let row = checkpoint_session(&pool, None, &riding_session(30))
            .await
            .unwrap();

        let mut finished = riding_session(45);
        finished.ended_at = Some(Utc::now());
        upsert_session(&pool, Some(row), &finished).await.unwrap();

        checkpoint_session(&pool, Some(row), &riding_session(31))
            .await
            .unwrap();

        assert!(load_unfinished_sessions(&pool).await.unwrap().is_empty());
        let records = load_session_records(&pool).await.unwrap();
        assert_eq!(records[0].session.data_points.len(), 45);
    }

    #[tokio::test]
    async fn a_summary_reports_the_same_tss_as_the_full_ride() {
        // The whole point of the stored columns: reading a ride without its
        // samples must not change any number the rider sees.
        let pool = test_pool().await;
        let mut session = riding_session(3600);
        session.started_at = Utc::now() - chrono::Duration::seconds(3600);
        session.ended_at = Some(Utc::now());
        save_session(&pool, &session).await.unwrap();

        let full = load_session_records(&pool).await.unwrap();
        let summaries = load_session_summaries(&pool).await.unwrap();

        let from_blob = full[0].session.tss(250).unwrap();
        let from_columns = summaries[0].tss(250).unwrap();
        assert!(
            (from_blob - from_columns).abs() < 0.5,
            "blob said {from_blob}, columns said {from_columns}"
        );
        assert_eq!(summaries[0].duration_secs, full[0].session.duration_secs());
    }

    #[tokio::test]
    async fn a_summary_scores_against_the_stamped_ftp() {
        let pool = test_pool().await;
        let mut session = riding_session(3600);
        session.started_at = Utc::now() - chrono::Duration::seconds(3600);
        session.ended_at = Some(Utc::now());
        session.ftp_watts = Some(200);
        save_session(&pool, &session).await.unwrap();

        let summaries = load_session_summaries(&pool).await.unwrap();
        // Ridden at 200 W FTP; the caller's 400 must not override the stamp.
        let expected = session.tss(400).unwrap();
        let actual = summaries[0].tss(400).unwrap();
        assert!((expected - actual).abs() < 0.5, "{expected} vs {actual}");
    }

    #[tokio::test]
    async fn metrics_are_backfilled_for_rides_written_before_the_columns_existed() {
        let pool = test_pool().await;
        let mut session = riding_session(3600);
        session.started_at = Utc::now() - chrono::Duration::seconds(3600);
        session.ended_at = Some(Utc::now());
        let id = save_session(&pool, &session).await.unwrap();

        // Simulate a row from before the migration.
        sqlx::query(
            "UPDATE sessions SET duration_secs = NULL, normalised_power = NULL,
                                 average_power = NULL, kilojoules = NULL WHERE id = ?",
        )
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
        assert!(load_session_summaries(&pool).await.unwrap()[0]
            .normalised_power
            .is_none());

        backfill_session_metrics(&pool).await.unwrap();

        let summaries = load_session_summaries(&pool).await.unwrap();
        assert!(summaries[0].normalised_power.is_some());
        assert_eq!(summaries[0].duration_secs, 3600);
    }

    #[tokio::test]
    async fn summaries_leave_out_a_ride_still_in_progress() {
        let pool = test_pool().await;
        checkpoint_session(&pool, None, &riding_session(60))
            .await
            .unwrap();
        assert!(load_session_summaries(&pool).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn session_title_survives_a_round_trip_and_names_the_activity() {
        let pool = test_pool().await;
        let mut session = Session::new(None);
        session.title = Some("Alpe d'Huez".into());
        // A finished ride: load_session_records lists completed rides only.
        session.ended_at = Some(Utc::now());
        save_session(&pool, &session).await.unwrap();

        let records = load_session_records(&pool).await.unwrap();
        assert_eq!(records[0].session.title.as_deref(), Some("Alpe d'Huez"));
        // A named ride must no longer read as "Unstructured Ride" in the calendar,
        // which takes its label from workout_name.
        assert_eq!(records[0].workout_name.as_deref(), Some("Alpe d'Huez"));
    }

    /// Store an Intervals.icu activity that mirrors `session`, as the sync would
    /// after the ride made the round trip through Garmin.
    async fn insert_icu_mirror(pool: &SqlitePool, icu_id: &str, session: &Session) {
        let local = session.started_at.with_timezone(&chrono::Local);
        upsert_intervals_activity(
            pool,
            icu_id,
            local.date_naive(),
            "Morning Ride",
            Some(80.0),
            Some(session.duration_secs() as u32),
            Some(200),
            Some(210),
            Some(140),
            Some(170),
            "Ride",
            Some(local.naive_local()),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    }

    fn hour_long_ride() -> Session {
        let mut s = Session::new(None);
        s.started_at = Utc::now() - chrono::Duration::hours(3);
        s.ended_at = Some(s.started_at + chrono::Duration::hours(1));
        s
    }

    #[tokio::test]
    async fn the_same_file_cannot_be_recorded_twice() {
        let pool = test_pool().await;
        save_route(&pool, "Alpe", "alpe-1.gpx", 1.0, 1.0)
            .await
            .unwrap();
        assert!(
            save_route(&pool, "Alpe again", "alpe-1.gpx", 1.0, 1.0)
                .await
                .is_err(),
            "file_name is unique, so one file backs at most one library entry"
        );
    }

    #[tokio::test]
    async fn reconcile_links_a_ride_that_came_back_from_intervals() {
        let pool = test_pool().await;
        let session = hour_long_ride();
        save_session(&pool, &session).await.unwrap();
        insert_icu_mirror(&pool, "icu-1", &session).await;

        assert_eq!(reconcile_icu_links(&pool).await.unwrap(), 1);

        let records = load_session_records(&pool).await.unwrap();
        assert_eq!(records[0].session.icu_id.as_deref(), Some("icu-1"));
        // The calendar hides the Intervals.icu copy, and load metrics skip the
        // local ride so the same stress is not counted twice.
        assert!(linked_icu_ids(&pool).await.unwrap().contains("icu-1"));
        assert!(records[0].summary().counted_via_intervals());
    }

    #[tokio::test]
    async fn a_linked_activity_is_hidden_from_everything_that_counts_rides() {
        let pool = test_pool().await;
        let session = hour_long_ride();
        save_session(&pool, &session).await.unwrap();
        insert_icu_mirror(&pool, "icu-1", &session).await;
        // A ride of its own, earlier the same day. It needs a start time of its
        // own: two rides sharing one to the second are indistinguishable from a
        // single ride uploaded twice, and would be collapsed as such.
        let mut other = hour_long_ride();
        other.started_at -= chrono::Duration::hours(4);
        other.ended_at = Some(other.started_at + chrono::Duration::hours(1));
        insert_icu_mirror(&pool, "icu-2", &other).await;
        reconcile_icu_links(&pool).await.unwrap();

        // The raw list still has both — reconciliation needs to see them.
        assert_eq!(load_intervals_activities(&pool).await.unwrap().len(), 2);
        // What the calendar, the fitness totals and the AI read has only the
        // ride that no local session already covers.
        let unlinked = load_unlinked_intervals_activities(&pool).await.unwrap();
        assert_eq!(unlinked.len(), 1);
        assert_eq!(unlinked[0].icu_id, "icu-2");
    }

    #[tokio::test]
    async fn a_ride_uploaded_three_times_is_still_one_ride() {
        // Re-exporting a ride to Intervals.icu gives each upload its own id, so
        // nothing downstream recognises the copies as one ride: it showed up
        // three times in the calendar and its TSS was counted three times, which
        // inflated fitness and fatigue for weeks afterwards.
        let pool = test_pool().await;
        let session = hour_long_ride();
        save_session(&pool, &session).await.unwrap();
        for icu_id in ["icu-1", "icu-2", "icu-3"] {
            insert_icu_mirror(&pool, icu_id, &session).await;
        }
        reconcile_icu_links(&pool).await.unwrap();

        // All three rows are still stored — reconciliation needs to see them.
        assert_eq!(load_intervals_activities(&pool).await.unwrap().len(), 3);

        // The local session covers the ride, so nothing is left to show.
        assert!(
            load_unlinked_intervals_activities(&pool)
                .await
                .unwrap()
                .is_empty(),
            "the ride is already on the calendar as a local session"
        );

        // One ride, one score — not three.
        let tss = load_intervals_tss_pairs(&pool).await.unwrap();
        assert_eq!(tss.len(), 1, "got {tss:?}");
        assert_eq!(tss[0].1, 80.0);

        assert_eq!(count_intervals_activities(&pool).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn a_duplicated_ride_with_no_local_session_still_shows_once() {
        // The same ride reaching Intervals.icu twice from elsewhere: there is no
        // local session to stand in for it, so exactly one copy must survive.
        let pool = test_pool().await;
        let ride = hour_long_ride();
        insert_icu_mirror(&pool, "icu-1", &ride).await;
        insert_icu_mirror(&pool, "icu-2", &ride).await;

        let unlinked = load_unlinked_intervals_activities(&pool).await.unwrap();
        assert_eq!(unlinked.len(), 1);
        assert_eq!(load_intervals_tss_pairs(&pool).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn reconcile_is_idempotent_and_leaves_unrelated_rides_alone() {
        let pool = test_pool().await;
        let session = hour_long_ride();
        save_session(&pool, &session).await.unwrap();
        insert_icu_mirror(&pool, "icu-1", &session).await;

        // A ride on another day must not be swept up.
        let mut other = hour_long_ride();
        other.started_at = Utc::now() - chrono::Duration::days(4);
        other.ended_at = Some(other.started_at + chrono::Duration::hours(1));
        save_session(&pool, &other).await.unwrap();

        assert_eq!(reconcile_icu_links(&pool).await.unwrap(), 1);
        // A second pass finds nothing new — links are not remade or duplicated.
        assert_eq!(reconcile_icu_links(&pool).await.unwrap(), 0);
        assert_eq!(linked_icu_ids(&pool).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn one_activity_cannot_be_claimed_by_two_sessions() {
        let pool = test_pool().await;
        let session = hour_long_ride();
        save_session(&pool, &session).await.unwrap();
        // A near-identical second recording of the same slot — only one may win.
        save_session(&pool, &session).await.unwrap();
        insert_icu_mirror(&pool, "icu-1", &session).await;

        assert_eq!(reconcile_icu_links(&pool).await.unwrap(), 1);
        let linked = load_session_records(&pool)
            .await
            .unwrap()
            .iter()
            .filter(|r| r.session.icu_id.is_some())
            .count();
        assert_eq!(linked, 1);
    }

    #[tokio::test]
    async fn unlinking_survives_the_next_sync() {
        let pool = test_pool().await;
        let session = hour_long_ride();
        let id = save_session(&pool, &session).await.unwrap();
        insert_icu_mirror(&pool, "icu-1", &session).await;
        reconcile_icu_links(&pool).await.unwrap();

        unlink_session_from_icu(&pool, id).await.unwrap();
        // The matcher would happily pair these again — the rider's decision wins.
        assert_eq!(reconcile_icu_links(&pool).await.unwrap(), 0);
        let records = load_session_records(&pool).await.unwrap();
        assert_eq!(records[0].session.icu_id, None);
        assert!(!records[0].summary().counted_via_intervals());
    }

    #[tokio::test]
    async fn backfill_links_a_historic_ride_the_everyday_matcher_misses() {
        let pool = test_pool().await;
        // A ride recorded before the fix: stamped 40 minutes before the rider
        // actually started, so its span is inflated at the front.
        let mut session = hour_long_ride();
        session.started_at -= chrono::Duration::minutes(40);
        save_session(&pool, &session).await.unwrap();

        // Intervals.icu has the real ride, starting 40 minutes into that span.
        let real_start = session.ended_at.unwrap() - chrono::Duration::hours(1);
        let local = real_start.with_timezone(&chrono::Local);
        upsert_intervals_activity(
            &pool,
            "icu-old",
            local.date_naive(),
            "Morning Ride",
            Some(80.0),
            Some(3600),
            Some(200),
            Some(210),
            Some(140),
            Some(170),
            "Ride",
            Some(local.naive_local()),
            None,
            None,
            None,
        )
        .await
        .unwrap();

        // The everyday pass cannot reach it — that is the whole reason for the backfill.
        assert_eq!(reconcile_icu_links(&pool).await.unwrap(), 0);
        assert_eq!(backfill_icu_links(&pool).await.unwrap(), 1);

        let records = load_session_records(&pool).await.unwrap();
        assert_eq!(records[0].session.icu_id.as_deref(), Some("icu-old"));
    }

    #[tokio::test]
    async fn backfill_runs_only_once() {
        let pool = test_pool().await;
        assert_eq!(backfill_icu_links(&pool).await.unwrap(), 0);
        assert_eq!(
            get_setting(&pool, LEGACY_BACKFILL_KEY).await.unwrap(),
            Some("1".into()),
            "the backfill must record that it has run"
        );

        // A ride added afterwards must not be swept up by a second pass.
        let session = hour_long_ride();
        save_session(&pool, &session).await.unwrap();
        insert_icu_mirror(&pool, "icu-1", &session).await;
        assert_eq!(backfill_icu_links(&pool).await.unwrap(), 0);
        // The everyday matcher still handles it, as it should.
        assert_eq!(reconcile_icu_links(&pool).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn backfill_respects_an_unlinked_ride() {
        let pool = test_pool().await;
        let session = hour_long_ride();
        let id = save_session(&pool, &session).await.unwrap();
        insert_icu_mirror(&pool, "icu-1", &session).await;
        unlink_session_from_icu(&pool, id).await.unwrap();

        assert_eq!(backfill_icu_links(&pool).await.unwrap(), 0);
        let records = load_session_records(&pool).await.unwrap();
        assert_eq!(records[0].session.icu_id, None);
    }

    #[tokio::test]
    async fn a_directly_uploaded_ride_is_still_excluded_from_load() {
        // The pre-existing upload path must keep working untouched.
        let pool = test_pool().await;
        let id = save_session(&pool, &hour_long_ride()).await.unwrap();
        mark_session_uploaded_to_icu(&pool, id).await.unwrap();

        let records = load_session_records(&pool).await.unwrap();
        assert!(records[0].summary().counted_via_intervals());
        assert_eq!(records[0].session.icu_id, None);
    }

    #[tokio::test]
    async fn clearing_a_title_restores_the_default_name() {
        let pool = test_pool().await;
        let mut session = Session::new(None);
        session.title = Some("Typo Ride".into());
        session.ended_at = Some(Utc::now());
        let id = save_session(&pool, &session).await.unwrap();

        set_session_title(&pool, id, "Evening Ride").await.unwrap();
        let records = load_session_records(&pool).await.unwrap();
        assert_eq!(records[0].workout_name.as_deref(), Some("Evening Ride"));

        // Blank input clears the name rather than storing an empty string.
        set_session_title(&pool, id, "   ").await.unwrap();
        let records = load_session_records(&pool).await.unwrap();
        assert_eq!(records[0].session.title, None);
        assert_eq!(records[0].workout_name, None);
    }

    #[tokio::test]
    async fn old_data_points_without_target_deserialise_as_none() {
        // JSON recorded before the target_watts field existed must still load.
        let json = r#"[{"elapsed_secs":0,"power_watts":200,"heart_rate_bpm":null,
                        "cadence_rpm":null,"speed_kmh":null}]"#;
        let points: Vec<DataPoint> = serde_json::from_str(json).unwrap();
        assert_eq!(points[0].power_watts, Some(200));
        assert_eq!(points[0].target_watts, None);
    }

    #[tokio::test]
    async fn save_session_rpe_updates_existing_row() {
        let pool = test_pool().await;
        let id = save_session(&pool, &Session::new(None)).await.unwrap();
        save_session_rpe(&pool, id, 7).await.unwrap();

        let rpe: Option<i64> = sqlx::query("SELECT rpe FROM sessions WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("rpe");
        assert_eq!(rpe, Some(7));
    }
}
