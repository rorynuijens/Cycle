//! Saved GPX routes, and the directory their files live in.

use anyhow::Result;
use chrono::Utc;
use sqlx::{Row, SqlitePool};

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
    let dir = crate::data::paths::data_dir().join("routes");
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::db::testing::*;

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
}
