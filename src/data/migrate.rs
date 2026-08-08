//! Schema versioning.
//!
//! The schema carries its version in SQLite's `user_version`. Changes from the
//! baseline onwards are numbered steps that run once, in order, each inside a
//! transaction that also carries its own version bump — so the recorded number
//! can never claim work that did not land.
//!
//! Steps fail loudly. The scheme this replaces applied additive `ALTER TABLE`s
//! with `.ok()`, which reported success whether the column had been added, was
//! already present, or the write had failed outright; a column that never
//! arrived then surfaced much later as a query error the rider saw as a toast,
//! a long way from the cause. The one benign case — the column is already
//! there — is now matched explicitly, and everything else propagates.

use anyhow::{bail, Context, Result};
use sqlx::error::DatabaseError;
use sqlx::SqlitePool;

/// The schema version this build understands.
///
/// Bump this when adding to [`MIGRATIONS`]; the test at the bottom of this file
/// fails if the two disagree.
pub const SCHEMA_VERSION: i32 = 1;

/// Version describing the schema as it stood before versioning existed.
///
/// A database at version 0 is either brand new or predates this module, and
/// [`establish_baseline`] brings both to this version.
const BASELINE_VERSION: i32 = 1;

/// A numbered schema change, applied to databases already at the baseline.
struct Migration {
    version: i32,
    /// Shown in the log and in the error when a step fails.
    name: &'static str,
    statements: &'static [&'static str],
}

/// Ordered schema changes from [`BASELINE_VERSION`] + 1 onwards.
///
/// Append only, never edit or reorder: a released step has already run on real
/// databases, and changing it makes the version number a lie about their shape.
const MIGRATIONS: &[Migration] = &[];

/// Bring `pool` up to [`SCHEMA_VERSION`], or fail explaining why it cannot be.
pub async fn run(pool: &SqlitePool) -> Result<()> {
    let found = user_version(pool).await?;

    if found > SCHEMA_VERSION {
        bail!(
            "This database was written by a newer version of Cycle: its schema is v{found}, \
             and this build understands v{SCHEMA_VERSION}. Update Cycle, or restore a backup \
             taken before the upgrade. Nothing has been changed."
        );
    }

    if found == 0 {
        // Every statement here is idempotent, so this needs no transaction: a run
        // interrupted halfway leaves the version at 0 and simply repeats next time.
        establish_baseline(pool)
            .await
            .context("could not establish the baseline schema")?;
        set_user_version(pool, BASELINE_VERSION).await?;
        tracing::info!("Schema baseline established at v{BASELINE_VERSION}");
    }

    let mut current = found.max(BASELINE_VERSION);
    for migration in MIGRATIONS {
        if migration.version <= current {
            continue;
        }
        apply(pool, migration).await?;
        current = migration.version;
    }

    if current != SCHEMA_VERSION {
        bail!("schema stopped at v{current} but this build expects v{SCHEMA_VERSION}");
    }

    tracing::info!("Schema is at v{current}");
    Ok(())
}

/// Apply one migration and record it, both or neither.
async fn apply(pool: &SqlitePool, migration: &Migration) -> Result<()> {
    tracing::info!(
        "Applying schema v{} ({})",
        migration.version,
        migration.name
    );
    let mut tx = pool.begin().await?;
    for statement in migration.statements {
        sqlx::query(statement)
            .execute(&mut *tx)
            .await
            .with_context(|| {
                format!(
                    "schema v{} ({}) failed on: {statement}",
                    migration.version, migration.name
                )
            })?;
    }
    set_user_version_tx(&mut tx, migration.version).await?;
    tx.commit().await?;
    Ok(())
}

/// Create every table and column of the pre-versioning schema that is missing.
///
/// Kept as `CREATE TABLE IF NOT EXISTS` plus additive `ALTER TABLE`s, rather
/// than folded into one set of complete definitions, because that is the shape
/// existing databases are already in: adopting one then does nothing but stamp
/// the version, which is far less to get wrong than reconciling two spellings
/// of the same schema.
///
/// A database created today can still end up with its columns in a different
/// order from one that grew into this shape over releases. That is harmless
/// here — no query in this crate uses `SELECT *` or reads a row by position.
async fn establish_baseline(pool: &SqlitePool) -> Result<()> {
    sqlx::query(BASELINE_TABLES)
        .execute(pool)
        .await
        .context("creating baseline tables")?;

    for statement in BASELINE_COLUMNS {
        add_column_if_missing(pool, statement).await?;
    }
    Ok(())
}

/// Run an additive `ALTER TABLE … ADD COLUMN`, tolerating only the case where
/// the column is already there.
///
/// Every other failure — a missing table, a locked or corrupt database, a full
/// disk — propagates.
async fn add_column_if_missing(pool: &SqlitePool, statement: &str) -> Result<()> {
    match sqlx::query(statement).execute(pool).await {
        Ok(_) => Ok(()),
        Err(sqlx::Error::Database(e)) if is_duplicate_column(e.as_ref()) => Ok(()),
        Err(e) => Err(anyhow::Error::new(e).context(format!("failed on: {statement}"))),
    }
}

/// SQLite reports an already-present column as "duplicate column name: x".
fn is_duplicate_column(e: &dyn DatabaseError) -> bool {
    e.message().contains("duplicate column name")
}

async fn user_version(pool: &SqlitePool) -> Result<i32> {
    let version: i32 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await
        .context("reading schema version")?;
    Ok(version)
}

// `PRAGMA user_version` takes no bind parameter, so the value has to be
// formatted in. It is an i32 from this file's own constants and never reaches
// here from a database row, a file, or the rider — the one place in this
// codebase where CLAUDE.md §5.2's rule against interpolation cannot apply.
async fn set_user_version(pool: &SqlitePool, version: i32) -> Result<()> {
    sqlx::query(&format!("PRAGMA user_version = {version}"))
        .execute(pool)
        .await
        .with_context(|| format!("recording schema version {version}"))?;
    Ok(())
}

async fn set_user_version_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    version: i32,
) -> Result<()> {
    sqlx::query(&format!("PRAGMA user_version = {version}"))
        .execute(&mut **tx)
        .await
        .with_context(|| format!("recording schema version {version}"))?;
    Ok(())
}

const BASELINE_TABLES: &str = r#"
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

    CREATE TABLE IF NOT EXISTS activity_streams (
        icu_id       TEXT PRIMARY KEY,
        streams_json TEXT NOT NULL,
        fetched_at   TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS time_off_entries (
        date  TEXT PRIMARY KEY,
        notes TEXT NOT NULL DEFAULT ''
    );
"#;

/// Columns added to the baseline tables across earlier releases.
const BASELINE_COLUMNS: &[&str] = &[
    "ALTER TABLE saved_devices ADD COLUMN erg_enabled INTEGER NOT NULL DEFAULT 1",
    "ALTER TABLE saved_devices ADD COLUMN device_type TEXT NOT NULL DEFAULT 'unknown'",
    "ALTER TABLE sessions ADD COLUMN rpe INTEGER",
    "ALTER TABLE sessions ADD COLUMN ftp_watts INTEGER",
    // Activity name — the route name for a GPX ride, or a name the rider typed.
    "ALTER TABLE sessions ADD COLUMN title TEXT",
    // Link to the same ride as it exists on Intervals.icu, so a ride that made the
    // round trip through Garmin or Strava is not shown or counted twice.
    "ALTER TABLE sessions ADD COLUMN icu_id TEXT",
    // Set when the rider unlinks a ride from Intervals.icu, so the matcher does not
    // simply pair them again on the next sync.
    "ALTER TABLE sessions ADD COLUMN icu_link_rejected INTEGER NOT NULL DEFAULT 0",
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
];

#[cfg(test)]
mod tests {
    use super::*;

    async fn empty_pool() -> SqlitePool {
        SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite should connect")
    }

    /// Columns of a table, as SQLite reports them.
    async fn columns(pool: &SqlitePool, table: &str) -> Vec<String> {
        sqlx::query_scalar(&format!("SELECT name FROM pragma_table_info('{table}')"))
            .fetch_all(pool)
            .await
            .expect("table should exist")
    }

    // ── the version number ───────────────────────────────────────────────────

    #[test]
    fn migrations_are_ordered_and_match_the_declared_version() {
        let mut previous = BASELINE_VERSION;
        for migration in MIGRATIONS {
            assert!(
                migration.version > previous,
                "migration v{} is out of order (after v{previous})",
                migration.version
            );
            previous = migration.version;
        }
        assert_eq!(
            previous, SCHEMA_VERSION,
            "SCHEMA_VERSION must equal the last migration's version"
        );
    }

    #[test]
    fn every_migration_does_something() {
        for migration in MIGRATIONS {
            assert!(
                !migration.statements.is_empty(),
                "migration v{} ({}) has no statements",
                migration.version,
                migration.name
            );
        }
    }

    // ── a fresh database ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn should_stamp_a_fresh_database_at_the_current_version() {
        let pool = empty_pool().await;
        assert_eq!(user_version(&pool).await.unwrap(), 0);

        run(&pool).await.expect("fresh migration should succeed");

        assert_eq!(user_version(&pool).await.unwrap(), SCHEMA_VERSION);
    }

    #[tokio::test]
    async fn should_give_a_fresh_database_every_late_added_column() {
        let pool = empty_pool().await;
        run(&pool).await.unwrap();

        let sessions = columns(&pool, "sessions").await;
        for expected in [
            "rpe",
            "ftp_watts",
            "title",
            "icu_id",
            "icu_link_rejected",
            "uploaded_to_icu",
            "duration_secs",
            "normalised_power",
            "average_power",
            "kilojoules",
        ] {
            assert!(
                sessions.iter().any(|c| c == expected),
                "sessions is missing {expected}"
            );
        }
    }

    #[tokio::test]
    async fn should_be_idempotent() {
        let pool = empty_pool().await;
        run(&pool).await.unwrap();
        let first = columns(&pool, "sessions").await;

        run(&pool).await.expect("a second run should be safe");
        run(&pool).await.expect("a third run should be safe");

        assert_eq!(user_version(&pool).await.unwrap(), SCHEMA_VERSION);
        assert_eq!(columns(&pool, "sessions").await, first);
    }

    // ── a database written before versioning existed ─────────────────────────

    #[tokio::test]
    async fn should_adopt_a_pre_versioning_database_without_losing_its_rows() {
        let pool = empty_pool().await;
        // The shape of the very first release: no version, and none of the
        // columns added later.
        sqlx::query(
            "CREATE TABLE sessions (
                 id               INTEGER PRIMARY KEY,
                 workout_id       INTEGER,
                 started_at       TEXT NOT NULL,
                 ended_at         TEXT,
                 data_points_json TEXT NOT NULL DEFAULT '[]'
             )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO sessions (started_at) VALUES ('2026-05-18T16:02:30+00:00')")
            .execute(&pool)
            .await
            .unwrap();

        run(&pool).await.expect("adopting a legacy database");

        assert_eq!(user_version(&pool).await.unwrap(), SCHEMA_VERSION);
        let kept: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(kept, 1, "the rider's ride must survive being adopted");
        assert!(columns(&pool, "sessions").await.iter().any(|c| c == "rpe"));
    }

    #[tokio::test]
    async fn should_adopt_a_complete_schema_that_carries_no_version() {
        // The state every real database was in on the day versioning arrived:
        // every table and column already present, `user_version` still 0. Each
        // baseline ALTER therefore hits "duplicate column name", which is the
        // one error that must not be treated as a failure.
        let pool = empty_pool().await;
        run(&pool).await.unwrap();
        let before = columns(&pool, "sessions").await;
        set_user_version(&pool, 0).await.unwrap();

        run(&pool)
            .await
            .expect("a complete v0 schema must be adopted");

        assert_eq!(user_version(&pool).await.unwrap(), SCHEMA_VERSION);
        assert_eq!(
            columns(&pool, "sessions").await,
            before,
            "adoption must not change the schema it adopts"
        );
    }

    // ── a database from a newer build ────────────────────────────────────────

    #[tokio::test]
    async fn should_refuse_a_database_from_a_newer_build() {
        let pool = empty_pool().await;
        run(&pool).await.unwrap();
        set_user_version(&pool, SCHEMA_VERSION + 1).await.unwrap();

        let err = run(&pool)
            .await
            .expect_err("a newer schema must be refused");

        let message = err.to_string();
        assert!(
            message.contains(&format!("v{}", SCHEMA_VERSION + 1)),
            "the error should name the version found: {message}"
        );
        assert!(
            message.contains("restore a backup"),
            "the error should tell the rider what to do: {message}"
        );
    }

    #[tokio::test]
    async fn should_leave_a_newer_database_untouched_when_refusing_it() {
        let pool = empty_pool().await;
        run(&pool).await.unwrap();
        set_user_version(&pool, SCHEMA_VERSION + 1).await.unwrap();

        run(&pool).await.expect_err("refused");

        assert_eq!(
            user_version(&pool).await.unwrap(),
            SCHEMA_VERSION + 1,
            "refusing must not rewrite the version it refused"
        );
    }

    // ── failing loudly ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn should_tolerate_a_column_that_is_already_present() {
        let pool = empty_pool().await;
        run(&pool).await.unwrap();

        add_column_if_missing(&pool, "ALTER TABLE sessions ADD COLUMN rpe INTEGER")
            .await
            .expect("an existing column is the one benign case");
    }

    #[tokio::test]
    async fn should_propagate_a_failure_that_is_not_a_duplicate_column() {
        let pool = empty_pool().await;

        // No such table: the old `.ok()` reported this as success.
        let err = add_column_if_missing(&pool, "ALTER TABLE nope ADD COLUMN x INTEGER")
            .await
            .expect_err("a missing table must not pass silently");

        assert!(
            err.to_string().contains("ALTER TABLE nope"),
            "the error should name the statement: {err}"
        );
    }

    #[tokio::test]
    async fn should_not_record_a_version_for_a_change_that_failed() {
        let pool = empty_pool().await;
        run(&pool).await.unwrap();

        // Stands in for a released migration whose second statement is wrong.
        let broken = Migration {
            version: SCHEMA_VERSION + 1,
            name: "deliberately broken",
            statements: &[
                "ALTER TABLE sessions ADD COLUMN landed INTEGER",
                "ALTER TABLE no_such_table ADD COLUMN x INTEGER",
            ],
        };

        apply(&pool, &broken).await.expect_err("should fail");

        assert_eq!(
            user_version(&pool).await.unwrap(),
            SCHEMA_VERSION,
            "a failed migration must not leave its version recorded"
        );
        assert!(
            !columns(&pool, "sessions")
                .await
                .iter()
                .any(|c| c == "landed"),
            "the first statement must roll back with the second"
        );
    }
}
