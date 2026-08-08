//! Snapshots taken before the schema changes underneath a rider's history.
//!
//! A schema change is the one moment a database is most likely to be damaged
//! and least likely to be recoverable: the rides, the FTP audit trail, the
//! wellness series and the plan exist in exactly one file, and nothing upstream
//! of it can put them back. So a copy is taken first, beside the database, and
//! the change does not proceed if the copy cannot be made.
//!
//! `VACUUM INTO` does the copying, rather than `std::fs::copy`, because a
//! write-ahead-log database is more than its main file: a plain byte copy of
//! `cycle.db` taken while a `-wal` holds uncheckpointed pages is not a
//! consistent database, which is the flaw in the hand-made `.bak` files this
//! replaces.

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use sqlx::{Row, SqlitePool};
use std::path::{Path, PathBuf};

/// How many generated snapshots to keep per database.
///
/// Older ones are pruned so a long-lived install does not accumulate copies of
/// a growing database indefinitely.
pub const KEEP_SNAPSHOTS: usize = 5;

/// Marks the files this module generates, so pruning can never reach a snapshot
/// somebody took by hand.
const SNAPSHOT_INFIX: &str = ".pre-";
const SNAPSHOT_SUFFIX: &str = ".bak";

/// Copy the database to a timestamped file beside itself.
///
/// Returns the path written, or `None` when there is nothing worth copying: an
/// in-memory database (every test) or one with no tables yet (a first run).
///
/// `label` names what the snapshot precedes, e.g. `"v2"`.
pub async fn snapshot(pool: &SqlitePool, label: &str) -> Result<Option<PathBuf>> {
    let Some(db_path) = main_db_path(pool).await? else {
        return Ok(None); // in-memory
    };
    if is_empty(pool).await? {
        return Ok(None); // nothing to lose yet
    }

    let file_name = db_path
        .file_name()
        .and_then(|n| n.to_str())
        .context("database path has no file name")?
        .to_owned();
    let dir = db_path.parent().unwrap_or(Path::new(".")).to_owned();
    let target = dir.join(snapshot_name(&file_name, label, Local::now()));

    vacuum_into(pool, &target)
        .await
        .with_context(|| format!("could not snapshot the database to {}", target.display()))?;

    tracing::info!("Database snapshot written: {}", target.display());

    // Pruning is a tidy-up, not part of the guarantee: a failure here must not
    // stop a migration that is already safe to run.
    if let Err(e) = prune(&dir, &file_name, KEEP_SNAPSHOTS).await {
        tracing::warn!("Could not prune old database snapshots: {e:#}");
    }

    Ok(Some(target))
}

/// Write a complete, consistent copy of the database to `target`.
///
/// `VACUUM INTO` refuses to overwrite an existing file, which is the behaviour
/// wanted in both callers: a name collision means something unexpected is going
/// on, and silently replacing a copy of somebody's history is the opposite of
/// the point.
pub(crate) async fn vacuum_into(pool: &SqlitePool, target: &Path) -> Result<()> {
    sqlx::query(&format!(
        "VACUUM INTO {}",
        quote_sql_string(&target.to_string_lossy())
    ))
    .execute(pool)
    .await?;
    Ok(())
}

/// The file backing the `main` database, or `None` when it is in memory.
pub(crate) async fn main_db_path(pool: &SqlitePool) -> Result<Option<PathBuf>> {
    let rows = sqlx::query("PRAGMA database_list")
        .fetch_all(pool)
        .await
        .context("asking SQLite where the database lives")?;

    for row in rows {
        let name: String = row.try_get("name").unwrap_or_default();
        if name != "main" {
            continue;
        }
        let file: String = row.try_get("file").unwrap_or_default();
        // SQLite reports an empty file for a temporary or in-memory database.
        return Ok(if file.is_empty() {
            None
        } else {
            Some(PathBuf::from(file))
        });
    }
    Ok(None)
}

/// True when the database holds no tables of ours yet.
async fn is_empty(pool: &SqlitePool) -> Result<bool> {
    let tables: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_one(pool)
    .await
    .context("counting tables")?;
    Ok(tables == 0)
}

/// Name for a snapshot of `db_file_name` taken before `label`.
///
/// Stamped in local time, because the name is read in a file manager next to
/// the modification times shown there, and a snapshot labelled with a UTC hour
/// is one a rider cannot match to when they made it.
fn snapshot_name(db_file_name: &str, label: &str, now: DateTime<Local>) -> String {
    format!(
        "{db_file_name}{SNAPSHOT_INFIX}{label}-{}{SNAPSHOT_SUFFIX}",
        now.format("%Y%m%d-%H%M%S")
    )
}

/// Delete all but the newest `keep` generated snapshots in `dir`.
async fn prune(dir: &Path, db_file_name: &str, keep: usize) -> Result<()> {
    let mut names = Vec::new();
    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        if let Some(name) = entry.file_name().to_str() {
            names.push(name.to_owned());
        }
    }

    for name in snapshots_to_prune(&names, db_file_name, keep) {
        let path = dir.join(&name);
        tokio::fs::remove_file(&path)
            .await
            .with_context(|| format!("removing {}", path.display()))?;
        tracing::info!("Pruned old database snapshot: {name}");
    }
    Ok(())
}

/// Which of `existing` to delete, newest `keep` retained.
///
/// Only ever selects names this module generates. A file somebody named
/// themselves — `cycle.db.bak-before-hr-repair-20260802-1401`, say — does not
/// match the pattern and is never returned.
fn snapshots_to_prune(existing: &[String], db_file_name: &str, keep: usize) -> Vec<String> {
    let prefix = format!("{db_file_name}{SNAPSHOT_INFIX}");
    let mut ours: Vec<&String> = existing
        .iter()
        .filter(|n| n.starts_with(&prefix) && n.ends_with(SNAPSHOT_SUFFIX))
        .collect();

    // The timestamp is fixed-width and big-endian, so lexical order is
    // chronological order within one label; sorting by the whole name keeps
    // snapshots for the same version together and the newest last.
    ours.sort();

    let excess = ours.len().saturating_sub(keep);
    ours.into_iter().take(excess).cloned().collect()
}

/// Quote a string for use where SQLite expects a literal and takes no bind
/// parameter, as `VACUUM INTO` does. Doubling embedded quotes is SQLite's own
/// escaping rule; the value is a path this process built, never rider input.
fn quote_sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(y: i32, m: u32, d: u32, hh: u32, mm: u32, ss: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, m, d, hh, mm, ss).unwrap()
    }

    // ── naming ───────────────────────────────────────────────────────────────

    #[test]
    fn should_name_a_snapshot_after_what_it_precedes() {
        let name = snapshot_name("cycle.db", "v2", at(2026, 8, 8, 9, 15, 0));
        assert_eq!(name, "cycle.db.pre-v2-20260808-091500.bak");
    }

    #[test]
    fn should_sort_snapshots_of_one_label_chronologically_by_name() {
        let early = snapshot_name("cycle.db", "v2", at(2026, 8, 8, 9, 15, 0));
        let late = snapshot_name("cycle.db", "v2", at(2026, 8, 8, 10, 2, 0));
        assert!(early < late, "{early} should sort before {late}");
    }

    // ── pruning ──────────────────────────────────────────────────────────────

    fn generated(label: &str, stamp: &str) -> String {
        format!("cycle.db.pre-{label}-{stamp}.bak")
    }

    #[test]
    fn should_keep_the_newest_and_prune_the_rest() {
        let names = vec![
            generated("v2", "20260801-100000"),
            generated("v2", "20260802-100000"),
            generated("v2", "20260803-100000"),
        ];
        let pruned = snapshots_to_prune(&names, "cycle.db", 2);
        assert_eq!(pruned, vec![generated("v2", "20260801-100000")]);
    }

    #[test]
    fn should_prune_nothing_when_under_the_limit() {
        let names = vec![generated("v2", "20260801-100000")];
        assert!(snapshots_to_prune(&names, "cycle.db", KEEP_SNAPSHOTS).is_empty());
    }

    #[test]
    fn should_never_prune_a_snapshot_taken_by_hand() {
        // The rider's own copies, and the shapes they actually take.
        let names = vec![
            "cycle.db.bak-2026-08-01".to_string(),
            "cycle.db.bak-before-hr-repair-20260802-1401".to_string(),
            "cycle.db-wal.bak-2026-08-01".to_string(),
            generated("v2", "20260801-100000"),
            generated("v2", "20260802-100000"),
        ];
        let pruned = snapshots_to_prune(&names, "cycle.db", 1);
        assert_eq!(
            pruned,
            vec![generated("v2", "20260801-100000")],
            "only this module's own files may be pruned"
        );
    }

    #[test]
    fn should_never_prune_the_database_or_its_sidecars() {
        let names = vec![
            "cycle.db".to_string(),
            "cycle.db-wal".to_string(),
            "cycle.db-shm".to_string(),
        ];
        assert!(snapshots_to_prune(&names, "cycle.db", 0).is_empty());
    }

    #[test]
    fn should_not_prune_snapshots_of_a_different_database() {
        let names = vec![
            "other.db.pre-v2-20260801-100000.bak".to_string(),
            "other.db.pre-v2-20260802-100000.bak".to_string(),
        ];
        assert!(snapshots_to_prune(&names, "cycle.db", 0).is_empty());
    }

    // ── quoting ──────────────────────────────────────────────────────────────

    #[test]
    fn should_quote_a_path_for_vacuum_into() {
        assert_eq!(quote_sql_string("/data/cycle.db"), "'/data/cycle.db'");
    }

    #[test]
    fn should_double_a_quote_inside_a_path() {
        // A directory may legally contain an apostrophe.
        assert_eq!(
            quote_sql_string("/home/o'brien/cycle.db"),
            "'/home/o''brien/cycle.db'"
        );
    }

    // ── against a real database ──────────────────────────────────────────────

    #[tokio::test]
    async fn should_skip_an_in_memory_database() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE t (x INTEGER)")
            .execute(&pool)
            .await
            .unwrap();

        let written = snapshot(&pool, "v2").await.unwrap();

        assert!(
            written.is_none(),
            "an in-memory database has no file to copy"
        );
    }

    #[tokio::test]
    async fn should_skip_a_database_with_no_tables() {
        let dir = tempdir();
        let pool = file_pool(&dir).await;

        let written = snapshot(&pool, "v1").await.unwrap();

        assert!(written.is_none(), "a first run has nothing worth copying");
    }

    #[tokio::test]
    async fn should_write_a_snapshot_that_still_holds_the_rows() {
        let dir = tempdir();
        let pool = file_pool(&dir).await;
        sqlx::query("CREATE TABLE sessions (id INTEGER PRIMARY KEY, started_at TEXT)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO sessions (started_at) VALUES ('2026-05-18T16:02:30+00:00')")
            .execute(&pool)
            .await
            .unwrap();

        let written = snapshot(&pool, "v2")
            .await
            .unwrap()
            .expect("a populated database is worth copying");

        assert!(written.exists(), "{} should exist", written.display());
        // Open the copy on its own and read the ride back out of it.
        let copy = SqlitePool::connect(&format!("sqlite://{}", written.display()))
            .await
            .unwrap();
        let kept: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
            .fetch_one(&copy)
            .await
            .unwrap();
        assert_eq!(kept, 1, "the snapshot must contain the ride");
        let version: i32 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&copy)
            .await
            .unwrap();
        assert_eq!(version, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn should_carry_uncheckpointed_wal_pages_into_the_snapshot() {
        // The flaw in a plain byte copy: rows still sitting in the -wal.
        let dir = tempdir();
        let pool = file_pool_wal(&dir).await;
        sqlx::query("CREATE TABLE sessions (id INTEGER PRIMARY KEY)")
            .execute(&pool)
            .await
            .unwrap();
        for _ in 0..50 {
            sqlx::query("INSERT INTO sessions DEFAULT VALUES")
                .execute(&pool)
                .await
                .unwrap();
        }

        let written = snapshot(&pool, "v2").await.unwrap().expect("snapshot");

        let copy = SqlitePool::connect(&format!("sqlite://{}", written.display()))
            .await
            .unwrap();
        let kept: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
            .fetch_one(&copy)
            .await
            .unwrap();
        assert_eq!(kept, 50, "every row must survive, WAL or not");
        std::fs::remove_dir_all(&dir).ok();
    }

    // Tests here need a database that is a real file, so they use a unique
    // directory under the system temp dir and remove it afterwards. This is
    // never the rider's XDG path (CLAUDE.md §3.5).
    fn tempdir() -> PathBuf {
        let unique = format!(
            "cycle-backup-test-{}-{}",
            std::process::id(),
            Local::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let dir = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    async fn file_pool(dir: &Path) -> SqlitePool {
        connect(dir, sqlx::sqlite::SqliteJournalMode::Delete).await
    }

    async fn file_pool_wal(dir: &Path) -> SqlitePool {
        connect(dir, sqlx::sqlite::SqliteJournalMode::Wal).await
    }

    async fn connect(dir: &Path, mode: sqlx::sqlite::SqliteJournalMode) -> SqlitePool {
        use std::str::FromStr;
        let path = dir.join("cycle.db");
        let options =
            sqlx::sqlite::SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
                .unwrap()
                .create_if_missing(true)
                .journal_mode(mode);
        SqlitePool::connect_with(options).await.expect("file db")
    }
}
