//! Remembered BLE and ANT+ devices.

use anyhow::Result;
use sqlx::{Row, SqlitePool};

#[derive(Debug, Clone)]
pub struct SavedDevice {
    pub address: String,
    pub display_name: String,
    pub transport: String,
    pub erg_enabled: bool,
    /// Device role as stored by `DeviceType::as_db_str` ("trainer", "hr", …).
    pub device_type: String,
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::db::testing::test_pool;

    fn addresses(list: &[SavedDevice]) -> Vec<&str> {
        list.iter().map(|d| d.address.as_str()).collect()
    }

    #[tokio::test]
    async fn should_remember_a_new_device_with_erg_on() {
        // ERG on by default: a trainer that silently ignores its targets is
        // the worse surprise.
        let pool = test_pool().await;
        save_device(&pool, "ant:12345", "Elite Drivo", "ant", "trainer")
            .await
            .unwrap();
        let d = &load_saved_devices(&pool).await.unwrap()[0];
        assert_eq!(d.address, "ant:12345");
        assert_eq!(d.display_name, "Elite Drivo");
        assert_eq!(d.transport, "ant");
        assert_eq!(d.device_type, "trainer");
        assert!(d.erg_enabled);
    }

    #[tokio::test]
    async fn should_keep_the_riders_name_when_the_device_connects_again() {
        let pool = test_pool().await;
        save_device(&pool, "ant:1", "HRM-Dual", "ant", "hr")
            .await
            .unwrap();
        rename_device(&pool, "ant:1", "Chest strap").await.unwrap();
        save_device(&pool, "ant:1", "HRM-Dual", "ant", "hr")
            .await
            .unwrap();
        assert_eq!(
            load_saved_devices(&pool).await.unwrap()[0].display_name,
            "Chest strap"
        );
    }

    #[tokio::test]
    async fn should_refresh_the_device_type_on_reconnect() {
        // Detection at connect time is the best source, so it wins.
        let pool = test_pool().await;
        save_device(&pool, "ble:aa", "Unknown", "ble", "unknown")
            .await
            .unwrap();
        save_device(&pool, "ble:aa", "Unknown", "ble", "cadence")
            .await
            .unwrap();
        let all = load_saved_devices(&pool).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].device_type, "cadence");
    }

    #[tokio::test]
    async fn should_keep_erg_off_once_switched_off_across_a_reconnect() {
        let pool = test_pool().await;
        save_device(&pool, "ant:1", "Drivo", "ant", "trainer")
            .await
            .unwrap();
        set_device_erg_enabled(&pool, "ant:1", false).await.unwrap();
        save_device(&pool, "ant:1", "Drivo", "ant", "trainer")
            .await
            .unwrap();
        assert!(!load_saved_devices(&pool).await.unwrap()[0].erg_enabled);
        set_device_erg_enabled(&pool, "ant:1", true).await.unwrap();
        assert!(load_saved_devices(&pool).await.unwrap()[0].erg_enabled);
    }

    #[tokio::test]
    async fn should_list_the_most_recently_seen_device_first() {
        let pool = test_pool().await;
        for a in ["ant:old", "ant:new", "ant:mid"] {
            save_device(&pool, a, a, "ant", "hr").await.unwrap();
        }
        // datetime('now') has one-second resolution, so set the times outright.
        for (a, t) in [
            ("ant:old", "2026-10-01 07:00:00"),
            ("ant:new", "2026-10-13 07:00:00"),
            ("ant:mid", "2026-10-09 07:00:00"),
        ] {
            sqlx::query("UPDATE saved_devices SET last_seen = ? WHERE address = ?")
                .bind(t)
                .bind(a)
                .execute(&pool)
                .await
                .unwrap();
        }
        assert_eq!(
            addresses(&load_saved_devices(&pool).await.unwrap()),
            ["ant:new", "ant:mid", "ant:old"]
        );
    }

    #[tokio::test]
    async fn should_forget_only_the_device_asked_for() {
        let pool = test_pool().await;
        save_device(&pool, "ant:1", "a", "ant", "hr").await.unwrap();
        save_device(&pool, "ant:2", "b", "ant", "trainer")
            .await
            .unwrap();
        delete_device(&pool, "ant:1").await.unwrap();
        assert_eq!(
            addresses(&load_saved_devices(&pool).await.unwrap()),
            ["ant:2"]
        );
    }

    #[tokio::test]
    async fn should_ignore_a_change_to_a_device_it_does_not_know() {
        let pool = test_pool().await;
        set_device_erg_enabled(&pool, "ant:nobody", false)
            .await
            .unwrap();
        rename_device(&pool, "ant:nobody", "x").await.unwrap();
        delete_device(&pool, "ant:nobody").await.unwrap();
        assert!(load_saved_devices(&pool).await.unwrap().is_empty());
    }
}
