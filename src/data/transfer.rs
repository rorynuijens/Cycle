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

/// What exporting every ride as a FIT file came to.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct RideExport {
    /// Files written.
    pub written: usize,
    /// Rides with no recorded samples, which make no FIT file worth having.
    pub empty: usize,
    /// Rides whose file could not be written.
    pub failed: usize,
}

/// Write every finished ride into `dir` as its own FIT file.
///
/// Files are named the way a single-ride export names them, so a ride saved
/// from the calendar and the same ride in an archive carry one name, and
/// exporting into the same folder again refreshes the archive rather than
/// doubling it. Two rides that would share a name in one export — the same
/// title started in the same minute — are told apart with a `-2` suffix.
///
/// Fails without writing anything if the profile or the rides cannot be read,
/// or if `dir` is not a folder. A file that cannot be written is counted in
/// [`RideExport::failed`] and the rest carry on.
pub async fn export_rides(pool: &SqlitePool, dir: &Path) -> Result<RideExport> {
    if !dir.is_dir() {
        bail!("{} is not a folder", dir.display());
    }
    // The training-load figure in each file is scaled to this profile, and
    // Garmin reads it rather than recomputing it — see the single-ride export.
    let athlete = super::db::load_or_create_athlete(pool)
        .await
        .context("could not read your profile")?;
    let mut records = super::db::load_session_records(pool)
        .await
        .context("could not read your rides")?;
    // Oldest first, so the earlier of two clashing rides keeps the plain name
    // and the archive's names do not shift when a newer ride is added.
    records.reverse();

    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut report = RideExport::default();
        let rides: Vec<_> = records
            .into_iter()
            .filter(|r| {
                let empty = r.session.data_points.is_empty();
                report.empty += empty as usize;
                !empty
            })
            .collect();
        let names = unique_file_names(
            rides
                .iter()
                .map(|r| {
                    super::fit::suggested_filename(
                        &r.session,
                        r.workout_name.as_deref().unwrap_or(""),
                    )
                })
                .collect(),
        );
        for (record, name) in rides.iter().zip(names) {
            match super::fit::write_session_fit(&dir.join(&name), &record.session, &athlete) {
                Ok(()) => report.written += 1,
                Err(e) => {
                    tracing::error!("session {}: FIT export failed: {e:#}", record.session.id);
                    report.failed += 1;
                }
            }
        }
        tracing::info!(
            "Exported {} rides as FIT ({} empty, {} failed)",
            report.written,
            report.empty,
            report.failed
        );
        report
    })
    .await
    .context("the export stopped unexpectedly")
}

/// Make every name in `names` distinct, keeping the first of each as it is and
/// numbering the rest `-2`, `-3`… before the extension.
///
/// Compared without case: an archive copied to a USB stick lands on FAT or
/// exFAT, where `Ride.fit` and `ride.fit` are the same file and the second
/// write would replace the first.
fn unique_file_names(names: Vec<String>) -> Vec<String> {
    let mut taken = std::collections::HashSet::new();
    names
        .into_iter()
        .map(|name| {
            let (stem, ext) = match name.rfind('.') {
                Some(dot) => name.split_at(dot),
                None => (name.as_str(), ""),
            };
            let mut candidate = name.clone();
            let mut n = 2;
            while !taken.insert(candidate.to_lowercase()) {
                candidate = format!("{stem}-{n}{ext}");
                n += 1;
            }
            candidate
        })
        .collect()
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
    use crate::data::paths::testing::ScratchDir;
    use chrono::TimeZone;

    fn tempdir() -> ScratchDir {
        ScratchDir::new("transfer-test")
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
        std::fs::remove_dir_all(&incoming_dir).ok();
    }

    // ── every ride as FIT ────────────────────────────────────────────────────

    /// Save a finished ride of `secs` one-second samples, titled `title`.
    async fn ride(pool: &SqlitePool, start: &str, title: Option<&str>, secs: u32) -> i64 {
        let mut s = crate::data::session::Session::new(None);
        s.started_at = DateTime::parse_from_rfc3339(start)
            .unwrap()
            .with_timezone(&chrono::Utc);
        s.ended_at = Some(s.started_at + chrono::Duration::seconds(secs as i64));
        s.title = title.map(str::to_string);
        s.data_points = (0..secs).map(sample).collect();
        crate::data::db::save_session(pool, &s).await.unwrap()
    }

    fn sample(elapsed_secs: u32) -> crate::data::session::DataPoint {
        crate::data::session::DataPoint {
            elapsed_secs,
            power_watts: Some(200),
            target_watts: None,
            heart_rate_bpm: None,
            cadence_rpm: None,
            speed_kmh: None,
            lat: None,
            lng: None,
            altitude_m: None,
        }
    }

    fn files_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn should_number_a_second_ride_that_would_share_a_name() {
        let names = vec!["a.fit".to_string(), "a.fit".into(), "a.fit".into()];
        assert_eq!(unique_file_names(names), ["a.fit", "a-2.fit", "a-3.fit"]);
    }

    #[test]
    fn should_treat_names_differing_only_in_case_as_a_clash() {
        // On FAT or exFAT these are one file, and the second write replaces the first.
        let names = vec!["Ride.fit".to_string(), "ride.fit".into()];
        assert_eq!(unique_file_names(names), ["Ride.fit", "ride-2.fit"]);
    }

    #[test]
    fn should_not_hand_out_a_numbered_name_a_later_ride_already_has() {
        // The numbered name for the first clash is a real name further down.
        let names = vec!["a.fit".to_string(), "a.fit".into(), "a-2.fit".into()];
        assert_eq!(unique_file_names(names), ["a.fit", "a-2.fit", "a-2-2.fit"]);
    }

    #[test]
    fn should_number_a_name_with_no_extension() {
        let names = vec!["ride".to_string(), "ride".into()];
        assert_eq!(unique_file_names(names), ["ride", "ride-2"]);
    }

    #[test]
    fn should_leave_distinct_names_alone() {
        assert!(unique_file_names(Vec::new()).is_empty());
        let names = vec!["a.fit".to_string(), "b.fit".into()];
        assert_eq!(unique_file_names(names), ["a.fit", "b.fit"]);
    }

    #[tokio::test]
    async fn should_write_one_readable_fit_file_per_ride() {
        let dir = tempdir();
        let pool = history(&dir, "cycle.db", 0).await;
        ride(&pool, "2026-05-01T10:00:00+00:00", Some("Threshold"), 60).await;
        ride(&pool, "2026-05-02T10:00:00+00:00", Some("Sweet Spot"), 90).await;
        let out = dir.join("out");
        std::fs::create_dir(&out).unwrap();

        let report = export_rides(&pool, &out).await.unwrap();

        assert_eq!(
            report,
            RideExport {
                written: 2,
                empty: 0,
                failed: 0
            }
        );
        let names = files_in(&out);
        assert_eq!(names.len(), 2);
        assert!(names[0].starts_with("Sweet_Spot-2026-05-02-"), "{names:?}");
        assert!(names[1].starts_with("Threshold-2026-05-01-"), "{names:?}");
        // Read back with the importer: the file must be a real activity, and
        // the right ride must be in the right file.
        let back = crate::data::fit::import_fit_file(&out.join(&names[0])).unwrap();
        assert_eq!(back.data_points.len(), 90);
    }

    #[tokio::test]
    async fn should_keep_both_rides_when_two_would_share_a_name() {
        // Same title, same minute: the second write would replace the first.
        let dir = tempdir();
        let pool = history(&dir, "cycle.db", 0).await;
        ride(&pool, "2026-05-01T10:00:05+00:00", Some("Ride"), 60).await;
        ride(&pool, "2026-05-01T10:00:40+00:00", Some("ride"), 120).await;
        let out = dir.join("out");
        std::fs::create_dir(&out).unwrap();

        let report = export_rides(&pool, &out).await.unwrap();

        assert_eq!(report.written, 2);
        let names = files_in(&out);
        assert_eq!(names.len(), 2, "{names:?}");
        // The earlier ride keeps the plain name.
        let plain = names.iter().find(|n| !n.ends_with("-2.fit")).unwrap();
        let first = crate::data::fit::import_fit_file(&out.join(plain)).unwrap();
        assert_eq!(first.data_points.len(), 60);
    }

    #[tokio::test]
    async fn should_skip_and_count_rides_with_no_samples() {
        let dir = tempdir();
        let pool = history(&dir, "cycle.db", 3).await; // three empty rides
        ride(&pool, "2026-06-01T10:00:00+00:00", Some("Real"), 30).await;
        let out = dir.join("out");
        std::fs::create_dir(&out).unwrap();

        let report = export_rides(&pool, &out).await.unwrap();

        assert_eq!(
            report,
            RideExport {
                written: 1,
                empty: 3,
                failed: 0
            }
        );
        assert_eq!(files_in(&out).len(), 1);
    }

    #[tokio::test]
    async fn should_write_nothing_for_an_empty_history() {
        let dir = tempdir();
        let pool = history(&dir, "cycle.db", 0).await;
        let out = dir.join("out");
        std::fs::create_dir(&out).unwrap();

        assert_eq!(
            export_rides(&pool, &out).await.unwrap(),
            RideExport::default()
        );
        assert!(files_in(&out).is_empty());
    }

    #[tokio::test]
    async fn should_leave_out_a_ride_still_in_progress() {
        // A checkpointed ride has no end: it is not history yet.
        let dir = tempdir();
        let pool = history(&dir, "cycle.db", 0).await;
        let mut live = crate::data::session::Session::new(None);
        live.data_points = (0..10).map(sample).collect();
        crate::data::db::checkpoint_session(&pool, None, &live)
            .await
            .unwrap();
        let out = dir.join("out");
        std::fs::create_dir(&out).unwrap();

        assert_eq!(export_rides(&pool, &out).await.unwrap().written, 0);
    }

    #[tokio::test]
    async fn should_refuse_a_target_that_is_not_a_folder() {
        let dir = tempdir();
        let pool = history(&dir, "cycle.db", 0).await;
        ride(&pool, "2026-05-01T10:00:00+00:00", Some("Threshold"), 60).await;
        let file = dir.join("a-file");
        std::fs::write(&file, b"x").unwrap();

        assert!(export_rides(&pool, &file).await.is_err());
        assert!(export_rides(&pool, &dir.join("missing")).await.is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"x");
    }

    #[tokio::test]
    async fn should_refresh_an_archive_rather_than_double_it() {
        let dir = tempdir();
        let pool = history(&dir, "cycle.db", 0).await;
        ride(&pool, "2026-05-01T10:00:00+00:00", Some("Threshold"), 60).await;
        let out = dir.join("out");
        std::fs::create_dir(&out).unwrap();

        export_rides(&pool, &out).await.unwrap();
        export_rides(&pool, &out).await.unwrap();

        assert_eq!(files_in(&out).len(), 1, "{:?}", files_in(&out));
    }

    #[tokio::test]
    async fn should_name_a_ride_with_a_turkish_title_and_no_title_at_all() {
        let dir = tempdir();
        let pool = history(&dir, "cycle.db", 0).await;
        ride(
            &pool,
            "2026-05-01T10:00:00+00:00",
            Some("İzmir – Çeşme"),
            30,
        )
        .await;
        ride(&pool, "2026-05-02T10:00:00+00:00", None, 30).await;
        let out = dir.join("out");
        std::fs::create_dir(&out).unwrap();

        assert_eq!(export_rides(&pool, &out).await.unwrap().written, 2);
        let names = files_in(&out);
        assert!(
            names.iter().any(|n| n.starts_with("İzmir___Çeşme-")),
            "{names:?}"
        );
        assert!(
            names.iter().any(|n| n.starts_with("Ride-2026-05-02-")),
            "{names:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn should_count_a_ride_it_could_not_write() {
        // A read-only folder: every write fails, and the report must say so
        // rather than claim an export that is not on disk.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir();
        let pool = history(&dir, "cycle.db", 0).await;
        ride(&pool, "2026-05-01T10:00:00+00:00", Some("A"), 30).await;
        ride(&pool, "2026-05-02T10:00:00+00:00", Some("B"), 30).await;
        let out = dir.join("out");
        std::fs::create_dir(&out).unwrap();
        std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o555)).unwrap();
        if std::fs::write(out.join("probe"), b"").is_ok() {
            return; // running as root: permissions do not bind, nothing to test
        }

        let report = export_rides(&pool, &out).await.unwrap();
        std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(
            report,
            RideExport {
                written: 0,
                empty: 0,
                failed: 2
            }
        );
    }
}
