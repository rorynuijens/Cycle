mod settings;
pub use settings::*;

mod goals;
pub use goals::*;

mod streams;
pub use streams::*;

mod routes;
pub use routes::*;

mod devices;
pub use devices::*;

mod athlete;
pub use athlete::*;

mod calendar;
pub use calendar::*;

mod wellness;
pub use wellness::*;

mod workouts;
pub use workouts::*;

mod intervals;
pub use intervals::*;

mod sessions;
pub use sessions::*;
// Not re-exported: `migrate` is the only caller.
use sessions::backfill_session_metrics;

use anyhow::Result;
use sqlx::{sqlite::SqliteConnectOptions, SqlitePool};
use std::str::FromStr;

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
    Ok(crate::data::paths::data_dir().join("cycle.db"))
}

/// Bring the schema up to date, then fill in anything derived that is missing.
///
/// Schema shape lives in [`crate::data::migrate`]; this function adds only the
/// data backfills, which are separate because they read and rewrite rows rather
/// than change the schema, and are safe to repeat.
async fn migrate(pool: &SqlitePool) -> Result<()> {
    crate::data::migrate::run(pool).await?;
    backfill_session_metrics(pool).await?;
    Ok(())
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

/// Fixtures shared by the tests in every `db` submodule.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use crate::data::workout::{Segment, Workout, WorkoutCategory};

    /// Build a fresh, fully-migrated in-memory database for each test.
    /// Never touches the real XDG path (see CLAUDE.md §3.5).
    pub(crate) async fn test_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite should connect");
        migrate(&pool).await.expect("migration should succeed");
        pool
    }

    pub(crate) fn sample_workout() -> Workout {
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
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[tokio::test]
    async fn migrate_is_idempotent() {
        let pool = test_pool().await;
        // Schema steps run once and are recorded; the backfill is guarded on the
        // column it fills. See crate::data::migrate for the schema-side tests.
        migrate(&pool).await.expect("re-running migration is safe");
    }
}
