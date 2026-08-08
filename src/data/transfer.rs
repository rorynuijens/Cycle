//! Taking a training history out of the app, and putting one back.
//!
//! The export is a real SQLite database rather than a bundle of JSON, for two
//! reasons. It is complete and exact — every table, every ride's per-second
//! stream, no serialiser standing between the rider and their own data — and
//! restoring it is a file copy rather than an importer that has to be kept in
//! step with the schema. Now that the schema carries its version, an export
//! also says which shape it is in, so an older file can be adopted and a newer
//! one refused instead of silently misread.
//!
//! Import is the dangerous direction: it replaces a history that exists in one
//! place. So a candidate is inspected before anything is touched, the current
//! database is copied first, and the app has to be restarted afterwards rather
//! than left running on a file that has been swapped underneath it.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Row, SqlitePool};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use super::backup;
use super::migrate::SCHEMA_VERSION;

/// Tables a file must have to be treated as a Cycle database at all.
///
/// Deliberately a small core rather than the full list: an export from an older
/// release is a legitimate thing to restore, and it will be missing whichever
/// tables arrived after it.
const REQUIRED_TABLES: &[&str] = &["athletes", "sessions", "workouts", "settings"];

/// What a candidate file turned out to contain.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportSummary {
    pub schema_version: i32,
    pub rides: i64,
    /// `YYYY-MM-DD` of the earliest and latest ride, when there is one.
    pub first_ride: Option<String>,
    pub last_ride: Option<String>,
    pub wellness_days: i64,
    pub workouts: i64,
}

impl ImportSummary {
    /// One line describing the span of riding in the file, for a dialog.
    pub fn ride_span(&self) -> String {
        match (&self.first_ride, &self.last_ride) {
            (Some(first), Some(last)) if first == last => {
                format!("{} ride, {first}", self.rides)
            }
            (Some(first), Some(last)) => {
                format!("{} rides, {first} to {last}", self.rides)
            }
            _ => "no rides".to_string(),
        }
    }
}

/// Default file name offered when exporting.
pub fn suggested_export_name(now: DateTime<Local>) -> String {
    format!("cycle-history-{}.db", now.format("%Y-%m-%d"))
}

/// Write the whole history to `target`, which must not already exist.
pub async fn export(pool: &SqlitePool, target: &Path) -> Result<()> {
    if target.exists() {
        // The file chooser already asks about replacing, and has removed the
        // file by the time it hands the path over; a file still here means
        // something else put it there between then and now.
        bail!("{} already exists", target.display());
    }
    backup::vacuum_into(pool, target)
        .await
        .with_context(|| format!("could not write the export to {}", target.display()))?;
    tracing::info!("History exported to {}", target.display());
    Ok(())
}

/// Open `path` read-only and decide whether it can be imported, without
/// changing it in any way.
pub async fn inspect(path: &Path) -> Result<ImportSummary> {
    let candidate = open_read_only(path).await?;
    let summary = inspect_pool(&candidate).await;
    candidate.close().await;
    summary
}

async fn open_read_only(path: &Path) -> Result<SqlitePool> {
    if !path.exists() {
        bail!("{} does not exist", path.display());
    }
    let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
        .with_context(|| format!("{} is not a usable database path", path.display()))?
        .create_if_missing(false)
        .read_only(true);
    SqlitePool::connect_with(options)
        .await
        .with_context(|| format!("{} could not be opened as a database", path.display()))
}

async fn inspect_pool(pool: &SqlitePool) -> Result<ImportSummary> {
    // Integrity first: everything below trusts the file's own structure, and a
    // corrupt page can otherwise surface as a confusing error much later.
    let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_one(pool)
        .await
        .context("this file could not be read as a database")?;
    if integrity != "ok" {
        bail!("this database is damaged ({integrity})");
    }

    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_all(pool)
    .await
    .context("listing tables")?;
    let missing: Vec<&str> = REQUIRED_TABLES
        .iter()
        .copied()
        .filter(|t| !tables.iter().any(|have| have == t))
        .collect();
    if !missing.is_empty() {
        bail!(
            "this does not look like a Cycle database (no {})",
            missing.join(", ")
        );
    }

    let schema_version: i32 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await
        .context("reading the schema version")?;
    if schema_version > SCHEMA_VERSION {
        bail!(
            "this export came from a newer version of Cycle (schema v{schema_version}; \
             this build understands v{SCHEMA_VERSION})"
        );
    }

    let row = sqlx::query(
        "SELECT COUNT(*) AS rides,
                MIN(substr(started_at, 1, 10)) AS first_ride,
                MAX(substr(started_at, 1, 10)) AS last_ride
           FROM sessions WHERE ended_at IS NOT NULL",
    )
    .fetch_one(pool)
    .await
    .context("counting rides")?;

    Ok(ImportSummary {
        schema_version,
        rides: row.try_get("rides").unwrap_or(0),
        first_ride: row.try_get("first_ride").ok().flatten(),
        last_ride: row.try_get("last_ride").ok().flatten(),
        wellness_days: count_if_present(pool, &tables, "wellness_entries").await,
        workouts: count_if_present(pool, &tables, "workouts").await,
    })
}

/// Row count for `table`, or 0 when an older export does not have it.
async fn count_if_present(pool: &SqlitePool, tables: &[String], table: &str) -> i64 {
    if !tables.iter().any(|t| t == table) {
        return 0;
    }
    sqlx::query_scalar(&format!("SELECT COUNT(*) FROM \"{table}\""))
        .fetch_one(pool)
        .await
        .unwrap_or(0)
}

/// Replace the live database with `candidate`.
///
/// Returns the path of the copy taken of what was replaced. The pool is closed
/// first and must not be used again: the file it was reading no longer exists,
/// and the caller is expected to restart the app.
///
/// Call [`inspect`] first — this does not re-validate.
pub async fn replace_with(pool: SqlitePool, candidate: &Path) -> Result<PathBuf> {
    // An import closes the pool, so a second one in the same run arrives here
    // with nothing to read. Say so plainly: the underlying error is "attempted
    // to acquire a connection on a closed pool", which tells a rider nothing.
    if pool.is_closed() {
        bail!("a history has already been imported in this session — restart Cycle first");
    }

    let db_path = backup::main_db_path(&pool)
        .await?
        .context("the current database is in memory and cannot be replaced")?;

    // Copy what is about to be overwritten, while the pool can still read it.
    let replaced = backup::snapshot(&pool, "import")
        .await
        .context("refusing to import without first copying the current history")?
        .context("the current database could not be copied")?;

    // Every connection has to be gone before the file moves, or SQLite will
    // write cached pages of the old database over the new one.
    pool.close().await;

    let staged = db_path.with_extension("db.importing");
    if let Err(e) = tokio::fs::copy(candidate, &staged).await {
        let _ = tokio::fs::remove_file(&staged).await;
        return Err(anyhow::Error::new(e).context(format!(
            "could not stage the import at {}",
            staged.display()
        )));
    }

    // Rename is atomic within a filesystem, so the live path is never a
    // half-written file: it is either the old database or the new one.
    if let Err(e) = tokio::fs::rename(&staged, &db_path).await {
        // Leaving a stray half-import beside the database would be mistaken for
        // part of the history later.
        let _ = tokio::fs::remove_file(&staged).await;
        return Err(anyhow::Error::new(e).context(format!(
            "could not move the import into {}",
            db_path.display()
        )));
    }

    // The old write-ahead log describes the old database. Left in place, SQLite
    // would replay those frames over the file just installed.
    remove_sidecars(&db_path).await;

    tracing::info!(
        "History replaced from {}; previous database kept at {}",
        candidate.display(),
        replaced.display()
    );
    Ok(replaced)
}

/// Delete the `-wal` and `-shm` companions of `db_path`, if present.
async fn remove_sidecars(db_path: &Path) {
    for suffix in ["-wal", "-shm"] {
        let mut name = db_path.as_os_str().to_owned();
        name.push(suffix);
        let sidecar = PathBuf::from(name);
        match tokio::fs::remove_file(&sidecar).await {
            Ok(()) => tracing::info!("Removed stale {}", sidecar.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!("Could not remove {}: {e}", sidecar.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn tempdir() -> PathBuf {
        let unique = format!(
            "cycle-transfer-test-{}-{}",
            std::process::id(),
            Local::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let dir = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// A file database with the real schema and `rides` finished rides.
    async fn history(dir: &Path, name: &str, rides: usize) -> SqlitePool {
        let path = dir.join(name);
        let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
            .unwrap()
            .create_if_missing(true);
        let pool = SqlitePool::connect_with(options).await.unwrap();
        crate::data::migrate::run(&pool).await.unwrap();
        for day in 0..rides {
            sqlx::query(
                "INSERT INTO sessions (started_at, ended_at, data_points_json)
                 VALUES (?, ?, '[]')",
            )
            .bind(format!("2026-05-{:02}T10:00:00+00:00", day + 1))
            .bind(format!("2026-05-{:02}T11:00:00+00:00", day + 1))
            .execute(&pool)
            .await
            .unwrap();
        }
        pool
    }

    // ── naming ───────────────────────────────────────────────────────────────

    #[test]
    fn should_name_an_export_after_the_day_it_was_taken() {
        let now = Local.with_ymd_and_hms(2026, 8, 8, 9, 15, 0).unwrap();
        assert_eq!(suggested_export_name(now), "cycle-history-2026-08-08.db");
    }

    // ── describing a candidate ───────────────────────────────────────────────

    #[test]
    fn should_describe_a_span_of_rides() {
        let s = ImportSummary {
            schema_version: 1,
            rides: 4,
            first_ride: Some("2026-05-18".into()),
            last_ride: Some("2026-08-02".into()),
            wellness_days: 110,
            workouts: 103,
        };
        assert_eq!(s.ride_span(), "4 rides, 2026-05-18 to 2026-08-02");
    }

    #[test]
    fn should_describe_a_single_ride_without_a_range() {
        let s = ImportSummary {
            schema_version: 1,
            rides: 1,
            first_ride: Some("2026-05-18".into()),
            last_ride: Some("2026-05-18".into()),
            wellness_days: 0,
            workouts: 0,
        };
        assert_eq!(s.ride_span(), "1 ride, 2026-05-18");
    }

    #[test]
    fn should_describe_an_empty_history() {
        let s = ImportSummary {
            schema_version: 1,
            rides: 0,
            first_ride: None,
            last_ride: None,
            wellness_days: 0,
            workouts: 0,
        };
        assert_eq!(s.ride_span(), "no rides");
    }

    // ── export ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn should_export_a_history_that_can_be_opened_on_its_own() {
        let dir = tempdir();
        let pool = history(&dir, "cycle.db", 3).await;
        let target = dir.join("out.db");

        export(&pool, &target).await.expect("export");

        let summary = inspect(&target).await.expect("the export is importable");
        assert_eq!(summary.rides, 3);
        assert_eq!(summary.schema_version, SCHEMA_VERSION);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn should_refuse_to_export_over_an_existing_file() {
        let dir = tempdir();
        let pool = history(&dir, "cycle.db", 1).await;
        let target = dir.join("taken.db");
        std::fs::write(&target, b"not mine").unwrap();

        let err = export(&pool, &target).await.expect_err("should refuse");

        assert!(err.to_string().contains("already exists"), "{err}");
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"not mine",
            "the existing file must be untouched"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── inspecting ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn should_reject_a_file_that_is_not_a_database() {
        let dir = tempdir();
        let path = dir.join("notes.db");
        std::fs::write(&path, b"just some text, definitely not sqlite").unwrap();

        let err = inspect(&path).await.expect_err("should be rejected");

        let message = err.to_string().to_lowercase();
        assert!(
            message.contains("database") || message.contains("read"),
            "{err}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn should_reject_a_database_that_is_not_cycles() {
        let dir = tempdir();
        let path = dir.join("other.db");
        let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
            .unwrap()
            .create_if_missing(true);
        let pool = SqlitePool::connect_with(options).await.unwrap();
        sqlx::query("CREATE TABLE recipes (id INTEGER PRIMARY KEY)")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;

        let err = inspect(&path).await.expect_err("should be rejected");

        assert!(
            err.to_string().contains("does not look like a Cycle"),
            "{err}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn should_reject_an_export_from_a_newer_build() {
        let dir = tempdir();
        let pool = history(&dir, "cycle.db", 1).await;
        sqlx::query(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1))
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;

        let err = inspect(&dir.join("cycle.db"))
            .await
            .expect_err("a newer schema must be refused");

        assert!(err.to_string().contains("newer version of Cycle"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn should_not_modify_the_file_it_inspects() {
        let dir = tempdir();
        let pool = history(&dir, "cycle.db", 2).await;
        pool.close().await;
        let path = dir.join("cycle.db");
        let before = std::fs::read(&path).unwrap();

        inspect(&path).await.expect("inspect");

        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "inspecting a candidate must leave it byte-identical"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── replacing ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn should_replace_the_live_history_and_keep_a_copy_of_the_old_one() {
        let dir = tempdir();
        let live = history(&dir, "cycle.db", 2).await;
        let incoming_dir = tempdir();
        let incoming = history(&incoming_dir, "incoming.db", 7).await;
        incoming.close().await;
        let candidate = incoming_dir.join("incoming.db");

        let replaced = replace_with(live, &candidate).await.expect("replace");

        // The live path now holds the imported history.
        let after = inspect(&dir.join("cycle.db")).await.unwrap();
        assert_eq!(after.rides, 7);
        // And what was there before is still readable.
        let old = inspect(&replaced).await.unwrap();
        assert_eq!(old.rides, 2, "the replaced history must be recoverable");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&incoming_dir).ok();
    }

    #[tokio::test]
    async fn should_leave_no_stale_write_ahead_log_behind() {
        let dir = tempdir();
        let path = dir.join("cycle.db");
        let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
            .unwrap()
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
        let live = SqlitePool::connect_with(options).await.unwrap();
        crate::data::migrate::run(&live).await.unwrap();
        sqlx::query("INSERT INTO sessions (started_at, data_points_json) VALUES ('x', '[]')")
            .execute(&live)
            .await
            .unwrap();

        let incoming_dir = tempdir();
        let incoming = history(&incoming_dir, "incoming.db", 3).await;
        incoming.close().await;

        replace_with(live, &incoming_dir.join("incoming.db"))
            .await
            .expect("replace");

        // A -wal describing the old database would be replayed over the new one.
        assert!(
            !dir.join("cycle.db-wal").exists(),
            "the old write-ahead log must not survive the import"
        );
        assert!(!dir.join("cycle.db-shm").exists());
        assert_eq!(inspect(&path).await.unwrap().rides, 3);
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&incoming_dir).ok();
    }

    #[tokio::test]
    async fn should_explain_itself_when_a_second_import_is_attempted() {
        // What happens in a real session: one import succeeds, closing the pool,
        // and the rider tries another before restarting. The raw error is
        // "attempted to acquire a connection on a closed pool".
        let dir = tempdir();
        let live = history(&dir, "cycle.db", 1).await;
        let incoming_dir = tempdir();
        let incoming = history(&incoming_dir, "incoming.db", 2).await;
        incoming.close().await;
        let candidate = incoming_dir.join("incoming.db");

        replace_with(live.clone(), &candidate).await.expect("first");
        let err = replace_with(live, &candidate)
            .await
            .expect_err("a second import cannot run on a closed pool");

        let message = err.to_string();
        assert!(
            message.contains("restart Cycle"),
            "the error should tell the rider what to do, got: {message}"
        );
        assert!(
            !message.contains("closed pool"),
            "the raw sqlx wording should not reach the rider: {message}"
        );
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&incoming_dir).ok();
    }

    #[tokio::test]
    async fn should_not_leave_a_staging_file_behind() {
        let dir = tempdir();
        let live = history(&dir, "cycle.db", 1).await;
        let incoming_dir = tempdir();
        let incoming = history(&incoming_dir, "incoming.db", 2).await;
        incoming.close().await;

        replace_with(live, &incoming_dir.join("incoming.db"))
            .await
            .expect("replace");

        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains("importing"))
            .collect();
        assert!(leftovers.is_empty(), "found {leftovers:?}");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&incoming_dir).ok();
    }
}
