use anyhow::Result;
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use sqlx::{sqlite::SqliteConnectOptions, Row, SqlitePool};
use std::str::FromStr;

use super::athlete::AthleteProfile;
use super::session::{DataPoint, Session};
use super::workout::{Segment, Workout, WorkoutCategory};

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

#[derive(Debug, Clone)]
pub struct SavedDevice {
    pub address: String,
    pub display_name: String,
    pub transport: String,
    pub erg_enabled: bool,
    /// Device role as stored by `DeviceType::as_db_str` ("trainer", "hr", …).
    pub device_type: String,
}

/// Open (or create) the SQLite database at the XDG data path and run migrations.
pub async fn open() -> Result<SqlitePool> {
    let db_path = xdg_data_path()?;
    tracing::info!("Database path: {}", db_path.display());

    if let Some(parent) = db_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);

    let pool = SqlitePool::connect_with(options).await?;
    migrate(&pool).await?;
    Ok(pool)
}

fn xdg_data_path() -> Result<std::path::PathBuf> {
    let base = std::env::var("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            std::path::PathBuf::from(home).join(".local/share")
        });
    Ok(base.join("cycle").join("cycle.db"))
}

async fn migrate(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS athletes (
            id          INTEGER PRIMARY KEY,
            name        TEXT    NOT NULL,
            weight_kg   REAL    NOT NULL DEFAULT 70.0,
            ftp_watts   INTEGER NOT NULL DEFAULT 200,
            max_hr      INTEGER NOT NULL DEFAULT 185,
            resting_hr  INTEGER NOT NULL DEFAULT 55
        );

        CREATE TABLE IF NOT EXISTS workouts (
            id              INTEGER PRIMARY KEY,
            name            TEXT    NOT NULL,
            description     TEXT    NOT NULL DEFAULT '',
            duration_secs   INTEGER NOT NULL,
            tss             REAL    NOT NULL DEFAULT 0,
            category        TEXT    NOT NULL DEFAULT 'Custom',
            segments_json   TEXT    NOT NULL DEFAULT '[]'
        );

        CREATE TABLE IF NOT EXISTS sessions (
            id               INTEGER PRIMARY KEY,
            workout_id       INTEGER REFERENCES workouts(id),
            started_at       TEXT    NOT NULL,
            ended_at         TEXT,
            data_points_json TEXT    NOT NULL DEFAULT '[]'
        );

        CREATE TABLE IF NOT EXISTS calendar_entries (
            id             INTEGER PRIMARY KEY,
            workout_id     INTEGER NOT NULL REFERENCES workouts(id),
            scheduled_date TEXT    NOT NULL,
            completed      INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS saved_devices (
            address      TEXT PRIMARY KEY,
            display_name TEXT NOT NULL,
            transport    TEXT NOT NULL DEFAULT 'ble',
            last_seen    TEXT
        );

        CREATE TABLE IF NOT EXISTS settings (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL DEFAULT ''
        );

        -- GPX routes kept in the library. The file itself lives under
        -- <data>/cycle/routes/; this table holds what the list needs so the page
        -- does not have to parse every GPX on every load.
        CREATE TABLE IF NOT EXISTS routes (
            id               INTEGER PRIMARY KEY,
            name             TEXT    NOT NULL,
            file_name        TEXT    NOT NULL UNIQUE,
            distance_m       REAL    NOT NULL DEFAULT 0,
            elevation_gain_m REAL    NOT NULL DEFAULT 0,
            added_at         TEXT    NOT NULL
        );

        CREATE TABLE IF NOT EXISTS ftp_history (
            id        INTEGER PRIMARY KEY,
            date      TEXT    NOT NULL,
            ftp_watts INTEGER NOT NULL,
            source    TEXT    NOT NULL DEFAULT 'manual',
            note      TEXT    NOT NULL DEFAULT ''
        );

        CREATE TABLE IF NOT EXISTS athlete_goals (
            id          INTEGER PRIMARY KEY,
            description TEXT NOT NULL,
            created_at  TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS intervals_activities (
            id            INTEGER PRIMARY KEY,
            icu_id        TEXT    UNIQUE NOT NULL,
            date          TEXT    NOT NULL,
            name          TEXT    NOT NULL DEFAULT '',
            tss           REAL,
            duration_secs INTEGER
        );

        CREATE TABLE IF NOT EXISTS intervals_workouts (
            id            INTEGER PRIMARY KEY,
            icu_id        TEXT    UNIQUE NOT NULL,
            name          TEXT    NOT NULL,
            description   TEXT    NOT NULL DEFAULT '',
            duration_secs INTEGER,
            tss           REAL
        );

        CREATE TABLE IF NOT EXISTS wellness_entries (
            date         TEXT PRIMARY KEY,
            hrv          REAL,
            resting_hr   INTEGER,
            sleep_secs   INTEGER,
            sleep_score  INTEGER,
            steps        INTEGER,
            calories     INTEGER
        );
        "#,
    )
    .execute(pool)
    .await?;

    // Additive columns added after initial release — safe to ignore if they already exist.
    sqlx::query("ALTER TABLE saved_devices ADD COLUMN erg_enabled INTEGER NOT NULL DEFAULT 1")
        .execute(pool)
        .await
        .ok();

    sqlx::query("ALTER TABLE saved_devices ADD COLUMN device_type TEXT NOT NULL DEFAULT 'unknown'")
        .execute(pool)
        .await
        .ok();

    sqlx::query("ALTER TABLE sessions ADD COLUMN rpe INTEGER")
        .execute(pool)
        .await
        .ok();

    sqlx::query("ALTER TABLE sessions ADD COLUMN ftp_watts INTEGER")
        .execute(pool)
        .await
        .ok();

    // Activity name — the route name for a GPX ride, or a name the rider typed.
    sqlx::query("ALTER TABLE sessions ADD COLUMN title TEXT")
        .execute(pool)
        .await
        .ok();

    // Link to the same ride as it exists on Intervals.icu, so a ride that made the
    // round trip through Garmin or Strava is not shown or counted twice.
    sqlx::query("ALTER TABLE sessions ADD COLUMN icu_id TEXT")
        .execute(pool)
        .await
        .ok();

    // Set when the rider unlinks a ride from Intervals.icu, so the matcher does not
    // simply pair them again on the next sync.
    sqlx::query("ALTER TABLE sessions ADD COLUMN icu_link_rejected INTEGER NOT NULL DEFAULT 0")
        .execute(pool)
        .await
        .ok();

    for col in [
        "ALTER TABLE intervals_activities ADD COLUMN average_watts REAL",
        "ALTER TABLE intervals_activities ADD COLUMN normalized_watts REAL",
        "ALTER TABLE intervals_activities ADD COLUMN max_watts INTEGER",
        "ALTER TABLE intervals_activities ADD COLUMN average_hr REAL",
        "ALTER TABLE intervals_activities ADD COLUMN sport_type TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE intervals_activities ADD COLUMN max_hr INTEGER",
        "ALTER TABLE intervals_activities ADD COLUMN start_datetime_local TEXT",
        "ALTER TABLE intervals_activities ADD COLUMN distance_m REAL",
        "ALTER TABLE intervals_activities ADD COLUMN elevation_gain_m REAL",
        "ALTER TABLE intervals_activities ADD COLUMN average_cadence REAL",
        // Tracks whether this in-app session was successfully uploaded to Intervals.icu.
        // Sessions marked as uploaded are excluded from local CTL calculation because
        // intervals_activities already contains the same workout — counting both would
        // inflate CTL/ATL by double-counting the same training stress.
        "ALTER TABLE sessions ADD COLUMN uploaded_to_icu INTEGER NOT NULL DEFAULT 0",
        // Ride metrics derived from data_points_json, stored so the list views
        // never have to read the blob back. Normalised power plus duration is
        // enough to recover TSS and IF against any FTP in constant time, so the
        // FTP a ride is scored at stays a read-time decision.
        "ALTER TABLE sessions ADD COLUMN duration_secs INTEGER",
        "ALTER TABLE sessions ADD COLUMN normalised_power REAL",
        "ALTER TABLE sessions ADD COLUMN average_power REAL",
        "ALTER TABLE sessions ADD COLUMN kilojoules REAL",
    ] {
        sqlx::query(col).execute(pool).await.ok();
    }

    backfill_session_metrics(pool).await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS activity_streams (
            icu_id       TEXT PRIMARY KEY,
            streams_json TEXT NOT NULL,
            fetched_at   TEXT NOT NULL
         )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS time_off_entries (
            date  TEXT PRIMARY KEY,
            notes TEXT NOT NULL DEFAULT ''
         )",
    )
    .execute(pool)
    .await?;

    tracing::info!("Database migrations complete");
    Ok(())
}

// ── Athlete ──────────────────────────────────────────────────────────────────

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

// ── Workouts ─────────────────────────────────────────────────────────────────

/// Insert a workout and return its new DB id.
pub async fn save_workout(pool: &SqlitePool, workout: &Workout) -> Result<i64> {
    let segments_json = serde_json::to_string(&workout.segments)?;
    let result = sqlx::query(
        "INSERT INTO workouts (name, description, duration_secs, tss, category, segments_json)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&workout.name)
    .bind(&workout.description)
    .bind(workout.duration_secs as i64)
    .bind(workout.tss as f64)
    .bind(workout.category.as_db_str())
    .bind(&segments_json)
    .execute(pool)
    .await?;
    Ok(result.last_insert_rowid())
}

pub async fn load_workouts(pool: &SqlitePool) -> Result<Vec<Workout>> {
    let rows = sqlx::query(
        "SELECT id, name, description, duration_secs, tss, category, segments_json
         FROM workouts ORDER BY id",
    )
    .fetch_all(pool)
    .await?;

    let mut workouts = Vec::new();
    for r in rows {
        let segments: Vec<Segment> = serde_json::from_str(r.get("segments_json"))?;
        let category_str: String = r.get("category");
        workouts.push(Workout {
            id: r.get("id"),
            name: r.get("name"),
            description: r.get("description"),
            duration_secs: r.get::<i64, _>("duration_secs") as u32,
            tss: r.get::<f64, _>("tss") as f32,
            category: WorkoutCategory::from_db_str(&category_str),
            segments,
        });
    }
    Ok(workouts)
}

/// Load a single workout by its primary-key id. Returns `None` if not found.
pub async fn load_workout_by_id(pool: &SqlitePool, id: i64) -> Result<Option<Workout>> {
    let row = sqlx::query(
        "SELECT id, name, description, duration_secs, tss, category, segments_json
         FROM workouts WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    match row {
        None => Ok(None),
        Some(r) => {
            let segments: Vec<Segment> = serde_json::from_str(r.get("segments_json"))?;
            let category_str: String = r.get("category");
            Ok(Some(Workout {
                id: r.get("id"),
                name: r.get("name"),
                description: r.get("description"),
                duration_secs: r.get::<i64, _>("duration_secs") as u32,
                tss: r.get::<f64, _>("tss") as f32,
                category: WorkoutCategory::from_db_str(&category_str),
                segments,
            }))
        }
    }
}

/// Update an existing workout (name, description, category, segments).
#[allow(dead_code)]
pub async fn update_workout(pool: &SqlitePool, workout: &Workout) -> Result<()> {
    let segments_json = serde_json::to_string(&workout.segments)?;
    sqlx::query(
        "UPDATE workouts
         SET name=?, description=?, duration_secs=?, tss=?, category=?, segments_json=?
         WHERE id=?",
    )
    .bind(&workout.name)
    .bind(&workout.description)
    .bind(workout.duration_secs as i64)
    .bind(workout.tss as f64)
    .bind(workout.category.as_db_str())
    .bind(&segments_json)
    .bind(workout.id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Delete a workout by id. Only call for custom (user-created) workouts.
pub async fn delete_workout(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query("DELETE FROM workouts WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Seed the workouts table with the full training library if not already seeded.
pub async fn seed_workouts(pool: &SqlitePool) -> Result<()> {
    let already_seeded: bool =
        sqlx::query("SELECT 1 FROM workouts WHERE name = 'Active Recovery 30' LIMIT 1")
            .fetch_optional(pool)
            .await?
            .is_some();

    if already_seeded {
        // Additive migration: insert any library workouts that were added after the
        // initial seed but are not yet present (matched by exact name).
        let new_workouts: &[&str] = &["Ramp Test", "20-Minute FTP Test"];
        for &name in new_workouts {
            let exists = sqlx::query("SELECT 1 FROM workouts WHERE name = ? LIMIT 1")
                .bind(name)
                .fetch_optional(pool)
                .await?
                .is_some();
            if !exists {
                if let Some(workout) = Workout::workout_library()
                    .into_iter()
                    .find(|w| w.name == name)
                {
                    save_workout(pool, &workout).await?;
                    tracing::info!("Migrated new workout: {name}");
                }
            }
        }
        return Ok(());
    }

    let library = Workout::workout_library();
    let count = library.len();
    for workout in library {
        save_workout(pool, &workout).await?;
    }
    tracing::info!("Seeded {count} workouts");
    Ok(())
}

// ── Sessions ─────────────────────────────────────────────────────────────────

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
async fn backfill_session_metrics(pool: &SqlitePool) -> Result<()> {
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
    Ok(load_intervals_activities(pool)
        .await?
        .into_iter()
        .filter(|a| !linked.contains(&a.icu_id))
        .collect())
}

/// As [`load_unlinked_intervals_activities`], for a date range.
pub async fn load_unlinked_intervals_activities_between(
    pool: &SqlitePool,
    start_date: &str,
    end_date: &str,
) -> Result<Vec<IntervalsActivity>> {
    let linked = linked_icu_ids(pool).await?;
    Ok(
        load_intervals_activities_between(pool, start_date, end_date)
            .await?
            .into_iter()
            .filter(|a| !linked.contains(&a.icu_id))
            .collect(),
    )
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

/// A GPX route saved in the library.
#[derive(Debug, Clone)]
pub struct SavedRoute {
    pub id: i64,
    pub name: String,
    /// File name within the routes directory — not a full path, so moving the
    /// data directory (or running under flatpak) does not orphan the entry.
    pub file_name: String,
    pub distance_m: f32,
    pub elevation_gain_m: f32,
}

/// Directory holding saved GPX files, created on first use.
pub fn routes_dir() -> Result<std::path::PathBuf> {
    let base = std::env::var("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            std::path::PathBuf::from(home).join(".local/share")
        });
    let dir = base.join("cycle").join("routes");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Record a saved route. `file_name` must already exist in [`routes_dir`].
pub async fn save_route(
    pool: &SqlitePool,
    name: &str,
    file_name: &str,
    distance_m: f32,
    elevation_gain_m: f32,
) -> Result<i64> {
    let result = sqlx::query(
        "INSERT INTO routes (name, file_name, distance_m, elevation_gain_m, added_at)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(name)
    .bind(file_name)
    .bind(distance_m as f64)
    .bind(elevation_gain_m as f64)
    .bind(Utc::now().to_rfc3339())
    .execute(pool)
    .await?;
    Ok(result.last_insert_rowid())
}

/// Every saved route, newest first.
pub async fn load_routes(pool: &SqlitePool) -> Result<Vec<SavedRoute>> {
    let rows = sqlx::query(
        "SELECT id, name, file_name, distance_m, elevation_gain_m
         FROM routes ORDER BY added_at DESC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| SavedRoute {
            id: r.get("id"),
            name: r.get("name"),
            file_name: r.get("file_name"),
            distance_m: r.get::<f64, _>("distance_m") as f32,
            elevation_gain_m: r.get::<f64, _>("elevation_gain_m") as f32,
        })
        .collect())
}

/// Rename a saved route. A blank name is ignored rather than clearing the title,
/// since a route with no name is unusable in the list.
pub async fn rename_route(pool: &SqlitePool, id: i64, name: &str) -> Result<()> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    sqlx::query("UPDATE routes SET name = ? WHERE id = ?")
        .bind(trimmed)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Forget a saved route and delete its GPX file.
///
/// The database row goes first: an entry pointing at a missing file is a broken
/// library, while a file with no entry is merely a few kilobytes wasted.
pub async fn delete_route(pool: &SqlitePool, id: i64) -> Result<()> {
    let row = sqlx::query("SELECT file_name FROM routes WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    sqlx::query("DELETE FROM routes WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    if let Some(row) = row {
        let file_name: String = row.get("file_name");
        if let Ok(dir) = routes_dir() {
            if let Err(e) = std::fs::remove_file(dir.join(&file_name)) {
                tracing::warn!("could not delete route file {file_name}: {e}");
            }
        }
    }
    Ok(())
}

// ── FTP history ───────────────────────────────────────────────────────────────
// Audit trail of FTP changes; consumed by FTP detection (docs/ftp-detection.md)
// for cooldown logic and the "Last updated" subtitle in Preferences.

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

/// Update the RPE score on an already-saved session.
pub async fn save_session_rpe(pool: &SqlitePool, session_id: i64, rpe: u8) -> Result<()> {
    sqlx::query("UPDATE sessions SET rpe = ? WHERE id = ?")
        .bind(rpe as i64)
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ── Saved devices ─────────────────────────────────────────────────────────────

pub async fn load_saved_devices(pool: &SqlitePool) -> Result<Vec<SavedDevice>> {
    let rows = sqlx::query(
        "SELECT address, display_name, transport, erg_enabled, device_type
         FROM saved_devices ORDER BY last_seen DESC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| SavedDevice {
            address: r.get("address"),
            display_name: r.get("display_name"),
            transport: r.get("transport"),
            erg_enabled: r.get::<i64, _>("erg_enabled") != 0,
            device_type: r.get("device_type"),
        })
        .collect())
}

/// Persist the ERG mode preference for a saved device.
pub async fn set_device_erg_enabled(pool: &SqlitePool, address: &str, enabled: bool) -> Result<()> {
    sqlx::query("UPDATE saved_devices SET erg_enabled = ? WHERE address = ?")
        .bind(enabled as i64)
        .bind(address)
        .execute(pool)
        .await?;
    Ok(())
}

/// Upsert a connected device. An existing custom `display_name` is preserved on conflict;
/// the device type is refreshed since connect-time detection is the most reliable source.
pub async fn save_device(
    pool: &SqlitePool,
    address: &str,
    display_name: &str,
    transport: &str,
    device_type: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO saved_devices (address, display_name, transport, device_type, last_seen)
         VALUES (?, ?, ?, ?, datetime('now'))
         ON CONFLICT(address) DO UPDATE SET
             last_seen = excluded.last_seen,
             device_type = excluded.device_type",
    )
    .bind(address)
    .bind(display_name)
    .bind(transport)
    .bind(device_type)
    .execute(pool)
    .await?;
    Ok(())
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
pub async fn schedule_workout(pool: &SqlitePool, workout_id: i64, date: &str) -> Result<i64> {
    let result = sqlx::query(
        "INSERT INTO calendar_entries (workout_id, scheduled_date, completed) VALUES (?, ?, 0)",
    )
    .bind(workout_id)
    .bind(date)
    .execute(pool)
    .await?;
    Ok(result.last_insert_rowid())
}

/// Persist changes to the (single) athlete profile row.
pub async fn update_athlete(pool: &SqlitePool, athlete: &AthleteProfile) -> Result<()> {
    sqlx::query(
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

pub async fn rename_device(pool: &SqlitePool, address: &str, new_name: &str) -> Result<()> {
    sqlx::query("UPDATE saved_devices SET display_name = ? WHERE address = ?")
        .bind(new_name)
        .bind(address)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete_device(pool: &SqlitePool, address: &str) -> Result<()> {
    sqlx::query("DELETE FROM saved_devices WHERE address = ?")
        .bind(address)
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

// ── Dashboard ─────────────────────────────────────────────────────────────────

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

// ── Session history ───────────────────────────────────────────────────────────

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

// ── Settings ──────────────────────────────────────────────────────────────────

/// Read a setting value by key. Returns `None` if the key has never been set.
pub async fn get_setting(pool: &SqlitePool, key: &str) -> Result<Option<String>> {
    let row = sqlx::query("SELECT value FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| r.get::<String, _>("value")))
}

/// A user-defined training goal.
#[derive(Debug, Clone)]
pub struct AthleteGoal {
    pub id: i64,
    pub description: String,
}

/// Upsert a setting value.
pub async fn set_setting(pool: &SqlitePool, key: &str, value: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO settings (key, value) VALUES (?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

// ── Athlete goals ─────────────────────────────────────────────────────────────

/// Persist a new training goal and return its id.
pub async fn save_goal(pool: &SqlitePool, description: &str) -> Result<i64> {
    let created_at = Utc::now().to_rfc3339();
    let result = sqlx::query("INSERT INTO athlete_goals (description, created_at) VALUES (?, ?)")
        .bind(description)
        .bind(&created_at)
        .execute(pool)
        .await?;
    Ok(result.last_insert_rowid())
}

/// Load all goals, newest first.
pub async fn load_goals(pool: &SqlitePool) -> Result<Vec<AthleteGoal>> {
    let rows = sqlx::query("SELECT id, description FROM athlete_goals ORDER BY id DESC")
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| AthleteGoal {
            id: r.get("id"),
            description: r.get("description"),
        })
        .collect())
}

/// Delete a goal by id.
pub async fn delete_goal(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query("DELETE FROM athlete_goals WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

// ── Intervals.icu activities ───────────────────────────────────────────────────

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
    let rows = sqlx::query(
        "SELECT date, tss FROM intervals_activities WHERE tss IS NOT NULL ORDER BY date",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let date_str: &str = r.get("date");
            let tss: f64 = r.get("tss");
            NaiveDate::parse_from_str(date_str, "%Y-%m-%d")
                .ok()
                .map(|d| (d, tss as f32))
        })
        .collect())
}

/// A full Intervals.icu activity row, used for display in History and AI coaching.
#[derive(Debug, Clone)]
#[allow(dead_code)]
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
    let row = sqlx::query("SELECT COUNT(*) AS cnt FROM intervals_activities")
        .fetch_one(pool)
        .await?;
    Ok(row.get("cnt"))
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

// ── Intervals.icu workouts ────────────────────────────────────────────────────

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

// ── Activity streams cache ────────────────────────────────────────────────────

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

// ── Wellness entries ──────────────────────────────────────────────────────────

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

fn build_activity_workout(
    name: &str,
    description: &str,
    duration_secs: u32,
    avg_pct: f32,
) -> super::workout::Workout {
    use super::workout::{Segment, WorkoutCategory};

    let wu_secs = (duration_secs as f32 * 0.10) as u32;
    let main_secs = (duration_secs as f32 * 0.80) as u32;
    let cd_secs = duration_secs.saturating_sub(wu_secs + main_secs);

    let segments = vec![
        Segment {
            duration_secs: wu_secs,
            power_low_pct: 50.0,
            power_high_pct: 50.0,
            label: None,
            cadence_target: None,
        },
        Segment {
            duration_secs: main_secs,
            power_low_pct: avg_pct,
            power_high_pct: avg_pct,
            label: Some("Main Effort".to_string()),
            cadence_target: None,
        },
        Segment {
            duration_secs: cd_secs,
            power_low_pct: 50.0,
            power_high_pct: 50.0,
            label: None,
            cadence_target: None,
        },
    ];

    let if_val = avg_pct / 100.0;
    let tss = (duration_secs as f32 / 3600.0) * (if_val * if_val) * 100.0;

    super::workout::Workout {
        id: 0,
        name: name.to_string(),
        description: description.to_string(),
        duration_secs,
        tss,
        category: WorkoutCategory::Custom,
        segments,
    }
}

/// Create a simple workout record derived from a past session (for scheduling) and return its id.
pub async fn create_workout_from_session(
    pool: &SqlitePool,
    session: &Session,
    workout_name: &str,
    ftp: u32,
) -> Result<i64> {
    let duration_secs = session.duration_secs() as u32;
    let avg_pct = if ftp > 0 {
        session
            .average_power()
            .map(|p| ((p / ftp as f32) * 100.0).clamp(40.0, 200.0))
            .unwrap_or(70.0)
    } else {
        70.0
    };
    let w = build_activity_workout(
        workout_name,
        "Auto-generated from a past session",
        duration_secs,
        avg_pct,
    );
    save_workout(pool, &w).await
}

/// Create a simple workout from an Intervals.icu activity summary.
pub async fn create_workout_from_icu_activity(
    pool: &SqlitePool,
    name: &str,
    duration_secs: u32,
    avg_watts: Option<u32>,
    ftp: u32,
) -> Result<i64> {
    let avg_pct = if ftp > 0 {
        avg_watts
            .map(|w| ((w as f32 / ftp as f32) * 100.0).clamp(40.0, 200.0))
            .unwrap_or(70.0)
    } else {
        70.0
    };
    let w = build_activity_workout(
        name,
        "Auto-generated from an Intervals.icu activity",
        duration_secs,
        avg_pct,
    );
    save_workout(pool, &w).await
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

// ── Time off entries ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct TimeOffEntry {
    pub date: NaiveDate,
    #[allow(dead_code)] // read by format_time_off_for_prompt when wired into AI prompts
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

// ── Intervals.icu activities (date-range variant) ─────────────────────────────

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

/// Migrate API keys from the plaintext settings table to the system keyring.
///
/// Called once at startup. If a key exists in the DB it is moved to the keyring and
/// removed from the DB. Subsequent reads and writes go through `data::keystore` directly.
pub async fn migrate_secrets_to_keyring(pool: &SqlitePool) -> Result<()> {
    let secret_keys = [
        super::keystore::KEY_ANTHROPIC,
        super::keystore::KEY_INTERVALS_API,
    ];
    for key in &secret_keys {
        match get_setting(pool, key).await? {
            Some(val) if !val.is_empty() => {
                if let Err(e) = super::keystore::set_secret(key, &val) {
                    tracing::warn!("keyring migration for {key} failed: {e} — key stays in DB");
                    continue;
                }
                sqlx::query("DELETE FROM settings WHERE key = ?")
                    .bind(*key)
                    .execute(pool)
                    .await?;
                tracing::debug!("Migrated {key} from DB to keyring");
            }
            _ => {}
        }
    }
    Ok(())
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
    use crate::data::session::{DataPoint, Session};
    use crate::data::workout::{Segment, Workout, WorkoutCategory};

    /// Build a fresh, fully-migrated in-memory database for each test.
    /// Never touches the real XDG path (see CLAUDE.md §3.5).
    async fn test_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite should connect");
        migrate(&pool).await.expect("migration should succeed");
        pool
    }

    fn sample_workout() -> Workout {
        Workout {
            id: 0,
            name: "Test Intervals".into(),
            description: "A test workout".into(),
            duration_secs: 600,
            tss: 42.0,
            category: WorkoutCategory::Threshold,
            segments: vec![
                Segment::steady(300, 60.0, "Warmup"),
                Segment::steady(300, 100.0, "Effort"),
            ],
        }
    }

    #[tokio::test]
    async fn migrate_is_idempotent() {
        let pool = test_pool().await;
        // Running migrations a second time must not error (CREATE IF NOT EXISTS + ignored ALTERs).
        migrate(&pool).await.expect("re-running migration is safe");
    }

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
    async fn save_and_load_workout_by_id_preserves_segments() {
        let pool = test_pool().await;
        let id = save_workout(&pool, &sample_workout()).await.unwrap();
        assert!(id > 0);

        let loaded = load_workout_by_id(&pool, id)
            .await
            .unwrap()
            .expect("workout should exist");
        assert_eq!(loaded.name, "Test Intervals");
        assert_eq!(loaded.category, WorkoutCategory::Threshold);
        assert_eq!(loaded.segments.len(), 2);
        assert_eq!(loaded.segments[1].power_low_pct, 100.0);
    }

    #[tokio::test]
    async fn load_workout_by_id_returns_none_for_missing_id() {
        let pool = test_pool().await;
        assert!(load_workout_by_id(&pool, 9999).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_workout_removes_the_row() {
        let pool = test_pool().await;
        let id = save_workout(&pool, &sample_workout()).await.unwrap();
        delete_workout(&pool, id).await.unwrap();
        assert!(load_workout_by_id(&pool, id).await.unwrap().is_none());
    }

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

    // ── Mid-ride checkpointing ───────────────────────────────────────────────

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

    // ── Stored ride metrics ──────────────────────────────────────────────────

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
    async fn routes_round_trip_newest_first() {
        let pool = test_pool().await;
        save_route(&pool, "Alpe", "alpe-1.gpx", 13_800.0, 1071.0)
            .await
            .unwrap();
        save_route(&pool, "Sea Wall", "seawall-1.gpx", 22_000.0, 15.0)
            .await
            .unwrap();

        let routes = load_routes(&pool).await.unwrap();
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].name, "Sea Wall", "newest first");
        assert_eq!(routes[1].file_name, "alpe-1.gpx");
        assert!((routes[1].distance_m - 13_800.0).abs() < 0.5);
        assert!((routes[1].elevation_gain_m - 1071.0).abs() < 0.5);
    }

    #[tokio::test]
    async fn renaming_a_route_sticks_and_ignores_a_blank_name() {
        let pool = test_pool().await;
        let id = save_route(&pool, "route", "r.gpx", 1.0, 1.0).await.unwrap();

        rename_route(&pool, id, "  Alpe d'Huez  ").await.unwrap();
        assert_eq!(
            load_routes(&pool).await.unwrap()[0].name,
            "Alpe d'Huez",
            "surrounding whitespace is trimmed"
        );

        rename_route(&pool, id, "   ").await.unwrap();
        assert_eq!(
            load_routes(&pool).await.unwrap()[0].name,
            "Alpe d'Huez",
            "a blank name leaves the route named as it was"
        );
    }

    #[tokio::test]
    async fn deleting_a_route_forgets_it() {
        let pool = test_pool().await;
        let id = save_route(&pool, "Alpe", "alpe-1.gpx", 13_800.0, 1071.0)
            .await
            .unwrap();
        // No file on disk here; deletion must still remove the row rather than
        // leaving a library entry pointing at nothing.
        delete_route(&pool, id).await.unwrap();
        assert!(load_routes(&pool).await.unwrap().is_empty());
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
        insert_icu_mirror(&pool, "icu-2", &hour_long_ride()).await;
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

    #[tokio::test]
    async fn settings_get_returns_none_then_upserts() {
        let pool = test_pool().await;
        assert!(get_setting(&pool, "theme").await.unwrap().is_none());

        set_setting(&pool, "theme", "dark").await.unwrap();
        assert_eq!(
            get_setting(&pool, "theme").await.unwrap().as_deref(),
            Some("dark")
        );

        // Upsert overwrites rather than duplicating the key.
        set_setting(&pool, "theme", "light").await.unwrap();
        assert_eq!(
            get_setting(&pool, "theme").await.unwrap().as_deref(),
            Some("light")
        );
    }

    #[tokio::test]
    async fn goals_save_load_and_delete() {
        let pool = test_pool().await;
        let first = save_goal(&pool, "Sub-60 FTP test").await.unwrap();
        save_goal(&pool, "Ride a century").await.unwrap();

        let goals = load_goals(&pool).await.unwrap();
        assert_eq!(goals.len(), 2);
        // Ordered newest first (id DESC).
        assert_eq!(goals[0].description, "Ride a century");

        delete_goal(&pool, first).await.unwrap();
        let remaining = load_goals(&pool).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].description, "Ride a century");
    }
}
