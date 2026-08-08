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
