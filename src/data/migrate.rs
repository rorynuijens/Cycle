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
pub const SCHEMA_VERSION: i32 = 5;

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
const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 2,
        name: "training programs",
        statements: &[
            // A program the rider is following. `active` is a flag rather than a
            // deletion so an abandoned plan keeps its calendar entries: the rides
            // were still planned, and the history should not rewrite itself.
            "CREATE TABLE programs (
             id            INTEGER PRIMARY KEY,
             created_at    TEXT    NOT NULL,
             start_monday  TEXT    NOT NULL,
             num_weeks     INTEGER NOT NULL,
             training_days TEXT    NOT NULL,
             active        INTEGER NOT NULL DEFAULT 1
         )",
            // Which program put this session on the calendar. NULL for anything
            // scheduled by hand or from a daily suggestion, which adaptation must
            // never touch.
            "ALTER TABLE calendar_entries
             ADD COLUMN program_id INTEGER REFERENCES programs(id)",
            // What the program originally asked for, stamped the first time an
            // entry is eased, so an adjustment can explain and undo itself.
            "ALTER TABLE calendar_entries
             ADD COLUMN original_workout_id INTEGER REFERENCES workouts(id)",
        ],
    },
    Migration {
        version: 3,
        name: "consolidate the daily AI brief",
        statements: &[
            // The morning briefing, the ride suggestion and the fitness insight are
            // now sections of one brief, cached under `ai.daily_brief`. None of
            // these can be carried across: they carry no date, no sections and no
            // verdict, so a migration could only guess — and guessing would restore
            // exactly the disagreement between cards this replaced. Dropping them
            // costs one launch with an empty Coach card, which the launch itself
            // fills.
            "DELETE FROM settings WHERE key IN (
             'ai.suggestion_response',
             'ai.suggestion_workout_name',
             'ai.suggestion_workout_detail',
             'ai.fitness_insight',
             'ai.morning_briefing_text',
             'ai.morning_briefing_date'
         )",
        ],
    },
    Migration {
        version: 4,
        name: "plan routes on the calendar",
        statements: &[
            // A planned day can now hold a GPX route instead of a workout, so
            // `workout_id` has to become nullable. SQLite cannot drop NOT NULL in
            // place, so the table is rebuilt. Nothing references `calendar_entries`,
            // which is what makes the drop safe; all four statements are one
            // migration so they commit or roll back together.
            //
            // The CHECK is the point of the rebuild: a row is a workout or a route,
            // never both and never neither, so no reader has to handle a third case.
            "CREATE TABLE calendar_entries_new (
             id                    INTEGER PRIMARY KEY,
             workout_id            INTEGER REFERENCES workouts(id),
             route_id              INTEGER REFERENCES routes(id),
             scheduled_date        TEXT    NOT NULL,
             completed             INTEGER NOT NULL DEFAULT 0,
             program_id            INTEGER REFERENCES programs(id),
             original_workout_id   INTEGER REFERENCES workouts(id),
             planned_tss           REAL,
             planned_duration_secs INTEGER,
             CHECK ((workout_id IS NOT NULL) <> (route_id IS NOT NULL))
         )",
            // Every existing row is a workout, so route_id and the planned_* columns
            // stay NULL. Ids are carried across deliberately: a program's sessions
            // and any open dialog refer to entries by id.
            "INSERT INTO calendar_entries_new
             (id, workout_id, scheduled_date, completed, program_id, original_workout_id)
         SELECT id, workout_id, scheduled_date, completed, program_id, original_workout_id
           FROM calendar_entries",
            "DROP TABLE calendar_entries",
            "ALTER TABLE calendar_entries_new RENAME TO calendar_entries",
            // Every calendar query filters on the date, and until now the schema
            // carried no indexes at all.
            "CREATE INDEX idx_calendar_entries_date ON calendar_entries(scheduled_date)",
        ],
    },
    Migration {
        version: 5,
        name: "step back one ease at a time",
        statements: &[
            // One row per ease actually applied, newest last. `original_workout_id`
            // stays and keeps naming where the chain started — the day still says
            // what the program first asked for, while Undo walks back a rung at a
            // time. Storing only the origin (what v2 did) collapses a chain of eases
            // to one value; storing only the chain loses the origin as soon as the
            // last step is undone. They are two different facts.
            //
            // Both cascades are load-bearing. On `entry_id`: SQLite reuses row ids,
            // so a chain outliving its deleted entry could later attach itself to a
            // new entry that happens to take the same id. On `from_workout_id`:
            // without it, a library workout that some day was once eased away from
            // could never be deleted again — blocking a delete to preserve an undo
            // rung is the wrong trade.
            "CREATE TABLE calendar_entry_adjustments (
             id              INTEGER PRIMARY KEY,
             entry_id        INTEGER NOT NULL REFERENCES calendar_entries(id) ON DELETE CASCADE,
             from_workout_id INTEGER NOT NULL REFERENCES workouts(id) ON DELETE CASCADE,
             applied_at      TEXT    NOT NULL
         )",
            "CREATE INDEX idx_entry_adjustments_entry ON calendar_entry_adjustments(entry_id, id)",
            // Carry an install that is mid-ease across as a one-step chain, so its
            // Undo keeps working. `scheduled_date` stands in for the applied time,
            // which was never recorded; it is only ever used for ordering, and one
            // row cannot be out of order.
            //
            // The NOT EXISTS makes a re-run a no-op. Every other statement in this
            // file is naturally idempotent and `is_already_applied` forgives the
            // ones that are not, but an unguarded INSERT would quietly double the
            // chain on a database re-migrated after being restored mid-upgrade.
            "INSERT INTO calendar_entry_adjustments (entry_id, from_workout_id, applied_at)
         SELECT ce.id, ce.original_workout_id, ce.scheduled_date
           FROM calendar_entries ce
          WHERE ce.original_workout_id IS NOT NULL
            AND NOT EXISTS (SELECT 1 FROM calendar_entry_adjustments a
                             WHERE a.entry_id = ce.id)",
        ],
    },
];

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
        // A database that already holds tables is being adopted, not created, and
        // the baseline may still add columns to it. Snapshot first; `snapshot`
        // returns None, and writes nothing, for a database that is genuinely new.
        crate::data::backup::snapshot(pool, &format!("v{BASELINE_VERSION}"))
            .await
            .context("refusing to touch the schema without a snapshot first")?;

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
///
/// The snapshot lives here rather than at the call site so that adding a
/// migration cannot forget it: a schema change without a copy of what it is
/// about to change is the case this whole module exists to prevent.
async fn apply(pool: &SqlitePool, migration: &Migration) -> Result<()> {
    tracing::info!(
        "Applying schema v{} ({})",
        migration.version,
        migration.name
    );
    crate::data::backup::snapshot(pool, &format!("v{}", migration.version))
        .await
        .with_context(|| {
            format!(
                "refusing to apply schema v{} without a snapshot first",
                migration.version
            )
        })?;

    let mut tx = pool.begin().await?;
    for statement in migration.statements {
        match sqlx::query(statement).execute(&mut *tx).await {
            Ok(_) => {}
            // Already in place — see `is_already_applied`. Logged rather than
            // passed over in silence, because on a database whose version was
            // accurate this would mean two migrations claim the same object.
            Err(sqlx::Error::Database(e)) if is_already_applied(e.as_ref()) => {
                tracing::warn!(
                    "schema v{} ({}): already applied, skipping: {statement}",
                    migration.version,
                    migration.name
                );
            }
            Err(e) => {
                return Err(anyhow::Error::new(e).context(format!(
                    "schema v{} ({}) failed on: {statement}",
                    migration.version, migration.name
                )))
            }
        }
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

/// SQLite reports a `CREATE TABLE` over an existing name as "table x already
/// exists".
fn is_existing_table(e: &dyn DatabaseError) -> bool {
    e.message().contains("already exists")
}

/// True for the two failures that mean "the database is already in the shape
/// this statement wants": the column is there, or the table is.
///
/// Nothing else is benign. A missing table, a locked or corrupt database, a
/// full disk or a syntax error all still propagate and abort the migration.
///
/// This tolerance exists because `user_version` and the schema can disagree:
/// a database restored from a backup, or interrupted between the write and the
/// version bump, arrives holding tables it is not credited with. Re-running the
/// step must then be a no-op rather than a hard failure that locks the rider
/// out of their own history.
fn is_already_applied(e: &dyn DatabaseError) -> bool {
    is_duplicate_column(e) || is_existing_table(e)
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
    use sqlx::Row;

    async fn empty_pool() -> SqlitePool {
        SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite should connect")
    }

    /// The migration declaring `version`, for tests that step through them.
    fn migration(version: i32) -> &'static Migration {
        MIGRATIONS
            .iter()
            .find(|m| m.version == version)
            .unwrap_or_else(|| panic!("migration v{version} should exist"))
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

    // ── v2: training programs ────────────────────────────────────────────────

    #[tokio::test]
    async fn should_add_the_programs_table_and_its_calendar_columns() {
        let pool = empty_pool().await;
        run(&pool).await.unwrap();

        let programs = columns(&pool, "programs").await;
        for expected in [
            "id",
            "created_at",
            "start_monday",
            "num_weeks",
            "training_days",
            "active",
        ] {
            assert!(
                programs.iter().any(|c| c == expected),
                "programs is missing {expected}"
            );
        }

        let entries = columns(&pool, "calendar_entries").await;
        for expected in ["program_id", "original_workout_id"] {
            assert!(
                entries.iter().any(|c| c == expected),
                "calendar_entries is missing {expected}"
            );
        }
    }

    #[tokio::test]
    async fn should_forget_the_cached_text_the_daily_brief_replaced() {
        // The suggestion, the fitness insight and the old morning briefing were
        // three separate answers with no shared date or verdict. Carrying them
        // forward would restore the disagreement between cards the brief exists
        // to remove, so v3 drops them.
        let pool = empty_pool().await;
        sqlx::query(BASELINE_TABLES).execute(&pool).await.unwrap();
        set_user_version(&pool, BASELINE_VERSION).await.unwrap();
        // Actually run v2 rather than just claiming its number: a later migration
        // reads columns v2 adds, and a fixture that lies about its version turns
        // that into a failure in this test rather than in the one at fault.
        apply(&pool, migration(2)).await.unwrap();

        let superseded = [
            "ai.suggestion_response",
            "ai.suggestion_workout_name",
            "ai.suggestion_workout_detail",
            "ai.fitness_insight",
            "ai.morning_briefing_text",
            "ai.morning_briefing_date",
        ];
        for key in superseded {
            sqlx::query("INSERT INTO settings (key, value) VALUES (?, 'stale')")
                .bind(key)
                .execute(&pool)
                .await
                .unwrap();
        }
        // A retrospective is a separate, still-current feature.
        sqlx::query("INSERT INTO settings (key, value) VALUES ('ai.weekly_retrospective', 'keep')")
            .execute(&pool)
            .await
            .unwrap();
        // And a rider's own setting must not be caught by the sweep.
        sqlx::query("INSERT INTO settings (key, value) VALUES ('intervals.athlete_id', 'i12345')")
            .execute(&pool)
            .await
            .unwrap();

        run(&pool).await.expect("upgrading from v2 should succeed");

        for key in superseded {
            let left: Option<String> =
                sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
                    .bind(key)
                    .fetch_optional(&pool)
                    .await
                    .unwrap();
            assert_eq!(left, None, "{key} should have been dropped");
        }

        let kept: Option<String> =
            sqlx::query_scalar("SELECT value FROM settings WHERE key = 'ai.weekly_retrospective'")
                .fetch_optional(&pool)
                .await
                .unwrap();
        assert_eq!(
            kept.as_deref(),
            Some("keep"),
            "retrospectives are not affected"
        );

        let setting: Option<String> =
            sqlx::query_scalar("SELECT value FROM settings WHERE key = 'intervals.athlete_id'")
                .fetch_optional(&pool)
                .await
                .unwrap();
        assert_eq!(
            setting.as_deref(),
            Some("i12345"),
            "a rider's settings are not cache"
        );
    }

    #[tokio::test]
    async fn should_carry_a_v1_database_to_the_current_version_keeping_its_calendar() {
        // The rider's own database is at v1 with a calendar already on it, so
        // this is the upgrade that will actually run in the field.
        let pool = empty_pool().await;
        sqlx::query(BASELINE_TABLES).execute(&pool).await.unwrap();
        set_user_version(&pool, 1).await.unwrap();
        sqlx::query(
            "INSERT INTO workouts (name, description, duration_secs, tss, category, segments_json)
             VALUES ('Sweet Spot', '', 3600, 60, 'SweetSpot', '[]')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO calendar_entries (workout_id, scheduled_date, completed)
             VALUES (1, '2026-06-16', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        run(&pool).await.expect("upgrading from v1 should succeed");

        // Against SCHEMA_VERSION rather than a literal, so adding a migration
        // does not make this test wrong about what it is checking.
        assert_eq!(user_version(&pool).await.unwrap(), SCHEMA_VERSION);
        let kept: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM calendar_entries")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(kept, 1, "the rider's planned session must survive");

        // An entry that predates programs belongs to none, which is what makes
        // it an orphan the card can offer to adopt.
        let program: Option<i64> =
            sqlx::query_scalar("SELECT program_id FROM calendar_entries WHERE id = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(program, None);
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
    // ── v4: planning routes on the calendar ──────────────────────────────────

    #[tokio::test]
    async fn should_carry_existing_plans_through_the_v4_table_rebuild() {
        // v4 rebuilds calendar_entries to make workout_id nullable, and the rebuild
        // drops the old table. This guards the one destructive step in the schema:
        // a planned session, the program that put it there, and the workout an
        // adjustment stamped on it must all survive, under the same row id.
        let pool = empty_pool().await;
        sqlx::query(BASELINE_TABLES).execute(&pool).await.unwrap();
        set_user_version(&pool, BASELINE_VERSION).await.unwrap();
        apply(&pool, migration(2)).await.unwrap();
        apply(&pool, migration(3)).await.unwrap();

        sqlx::query("INSERT INTO workouts (name, duration_secs) VALUES ('Threshold', 3600)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO programs (created_at, start_monday, num_weeks, training_days)
             VALUES ('2026-01-01', '2026-01-05', 4, 'monday')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO calendar_entries
                 (workout_id, scheduled_date, completed, program_id, original_workout_id)
             VALUES (1, '2026-03-04', 1, 1, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        apply(&pool, migration(4)).await.unwrap();

        let row = sqlx::query(
            "SELECT id, workout_id, route_id, scheduled_date, completed,
                    program_id, original_workout_id, planned_tss
               FROM calendar_entries",
        )
        .fetch_one(&pool)
        .await
        .expect("the planned session must survive the rebuild");

        assert_eq!(row.get::<i64, _>("id"), 1, "row ids must be carried across");
        assert_eq!(row.get::<i64, _>("workout_id"), 1);
        assert_eq!(row.get::<Option<i64>, _>("route_id"), None);
        assert_eq!(row.get::<String, _>("scheduled_date"), "2026-03-04");
        assert_eq!(row.get::<i64, _>("completed"), 1);
        assert_eq!(row.get::<Option<i64>, _>("program_id"), Some(1));
        assert_eq!(row.get::<Option<i64>, _>("original_workout_id"), Some(1));
        assert_eq!(
            row.get::<Option<f64>, _>("planned_tss"),
            None,
            "a migrated workout row carries no stored estimate"
        );
        assert_eq!(user_version(&pool).await.unwrap(), 4);
    }

    /// Walk a database up to v4 with one eased entry on it, ready for v5.
    async fn pool_at_v4_with_an_eased_entry() -> SqlitePool {
        let pool = empty_pool().await;
        sqlx::query(BASELINE_TABLES).execute(&pool).await.unwrap();
        set_user_version(&pool, BASELINE_VERSION).await.unwrap();
        for version in [2, 3, 4] {
            apply(&pool, migration(version)).await.unwrap();
        }
        sqlx::query("INSERT INTO workouts (name, duration_secs) VALUES ('Threshold 3x10', 3600)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO workouts (name, duration_secs) VALUES ('VO2Max Blocks', 3300)")
            .execute(&pool)
            .await
            .unwrap();
        // Eased once already: it now holds Threshold, and the program asked for VO2Max.
        sqlx::query(
            "INSERT INTO calendar_entries (workout_id, scheduled_date, original_workout_id)
             VALUES (1, '2026-03-04', 2)",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    #[tokio::test]
    async fn should_carry_an_entry_already_eased_into_the_new_chain() {
        // An install upgrading mid-ease must keep its Undo working: v5 reads the
        // step from the chain table, so a single `original_workout_id` that predates
        // the table has to arrive in it.
        let pool = pool_at_v4_with_an_eased_entry().await;

        apply(&pool, migration(5)).await.unwrap();

        let rows: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT entry_id, from_workout_id FROM calendar_entry_adjustments ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            vec![(1, 2)],
            "the one ease already applied becomes a one-step chain back to VO2Max Blocks"
        );

        let original: Option<i64> =
            sqlx::query_scalar("SELECT original_workout_id FROM calendar_entries")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            original,
            Some(2),
            "the origin column stays: the day still names what the program first asked for"
        );
        assert_eq!(user_version(&pool).await.unwrap(), 5);
    }

    #[tokio::test]
    async fn should_not_double_the_chain_when_v5_is_applied_twice() {
        // `is_already_applied` deliberately lets a re-run through, which a database
        // restored between the write and the version bump will do. Every other
        // statement in v5 is idempotent on its own; the backfill only is because of
        // its NOT EXISTS, and this is the test that says so.
        let pool = pool_at_v4_with_an_eased_entry().await;

        apply(&pool, migration(5)).await.unwrap();
        apply(&pool, migration(5)).await.unwrap();

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM calendar_entry_adjustments")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1, "the second run must add nothing");
    }

    #[tokio::test]
    async fn should_drop_the_ease_chain_when_its_planned_day_is_deleted() {
        // SQLite reuses row ids. A chain outliving its entry would be inherited by
        // whichever new entry next took that id, so the day would open already
        // offering to undo an ease that was applied to something else.
        let pool = empty_pool().await;
        run(&pool).await.unwrap();
        sqlx::query("INSERT INTO workouts (name, duration_secs) VALUES ('Threshold', 3600)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO calendar_entries (workout_id, scheduled_date) VALUES (1, '2026-03-04')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO calendar_entry_adjustments (entry_id, from_workout_id, applied_at)
             VALUES (1, 1, '2026-03-04')",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query("DELETE FROM calendar_entries WHERE id = 1")
            .execute(&pool)
            .await
            .unwrap();

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM calendar_entry_adjustments")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "the chain goes with the day");
    }

    #[tokio::test]
    async fn should_let_a_workout_once_eased_away_from_still_be_deleted() {
        // The plan protects the workout an entry currently holds, but a workout it
        // has moved off is only named by the chain. Blocking a library delete to
        // preserve an undo rung would be the wrong trade.
        let pool = empty_pool().await;
        run(&pool).await.unwrap();
        for name in ["Threshold", "Sweet Spot"] {
            sqlx::query("INSERT INTO workouts (name, duration_secs) VALUES (?, 3600)")
                .bind(name)
                .execute(&pool)
                .await
                .unwrap();
        }
        // The entry now holds Sweet Spot, having been eased down from Threshold.
        sqlx::query(
            "INSERT INTO calendar_entries (workout_id, scheduled_date) VALUES (2, '2026-03-04')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO calendar_entry_adjustments (entry_id, from_workout_id, applied_at)
             VALUES (1, 1, '2026-03-04')",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query("DELETE FROM workouts WHERE id = 1")
            .execute(&pool)
            .await
            .expect("a workout only an undo step names must stay deletable");

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM calendar_entry_adjustments")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "the rung goes with the workout it named");
    }

    #[tokio::test]
    async fn should_leave_the_chain_empty_for_a_plan_that_was_never_eased() {
        let pool = empty_pool().await;
        run(&pool).await.unwrap();
        sqlx::query("INSERT INTO workouts (name, duration_secs) VALUES ('Endurance', 3600)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO calendar_entries (workout_id, scheduled_date) VALUES (1, '2026-03-04')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM calendar_entry_adjustments")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn should_refuse_a_calendar_entry_that_is_both_a_workout_and_a_route() {
        let pool = empty_pool().await;
        run(&pool).await.unwrap();
        sqlx::query("INSERT INTO workouts (name, duration_secs) VALUES ('W', 3600)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO routes (name, file_name, distance_m, elevation_gain_m, added_at)
             VALUES ('R', 'r.gpx', 1000, 10, '2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO calendar_entries (workout_id, route_id, scheduled_date)
             VALUES (1, 1, '2026-03-04')",
        )
        .execute(&pool)
        .await
        .expect_err("a row may name a workout or a route, never both");
    }

    #[tokio::test]
    async fn should_refuse_a_calendar_entry_that_names_neither() {
        let pool = empty_pool().await;
        run(&pool).await.unwrap();

        sqlx::query("INSERT INTO calendar_entries (scheduled_date) VALUES ('2026-03-04')")
            .execute(&pool)
            .await
            .expect_err("a row with no workout and no route is not a plan");
    }
}
