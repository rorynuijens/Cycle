//! Raw key/value rows. Typed access lives in `crate::data::settings` and `crate::data::ai_cache`.

use anyhow::Result;
use sqlx::{Row, SqlitePool};

/// Read a setting value by key. Returns `None` if the key has never been set.
pub async fn get_setting(pool: &SqlitePool, key: &str) -> Result<Option<String>> {
    let row = sqlx::query("SELECT value FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| r.get::<String, _>("value")))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::db::testing::*;

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
}
