//! The rider's training goals.

use anyhow::Result;
use chrono::Utc;
use sqlx::{Row, SqlitePool};

/// A user-defined training goal.
#[derive(Debug, Clone)]
pub struct AthleteGoal {
    pub id: i64,
    pub description: String,
}

/// Persist a new training goal and return its id.
pub async fn save_goal(pool: &SqlitePool, description: &str) -> Result<i64> {
    let created_at = Utc::now().to_rfc3339();
    let result = sqlx::query("INSERT INTO athlete_goals (description, created_at) VALUES (?, ?)")
        .bind(description)
        .bind(&created_at)
        .execute(pool)
        .await?;
    Ok(result.last_insert_rowid())
}

/// Load all goals, newest first.
pub async fn load_goals(pool: &SqlitePool) -> Result<Vec<AthleteGoal>> {
    let rows = sqlx::query("SELECT id, description FROM athlete_goals ORDER BY id DESC")
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| AthleteGoal {
            id: r.get("id"),
            description: r.get("description"),
        })
        .collect())
}

/// Delete a goal by id.
pub async fn delete_goal(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query("DELETE FROM athlete_goals WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::db::testing::*;

    #[tokio::test]
    async fn goals_save_load_and_delete() {
        let pool = test_pool().await;
        let first = save_goal(&pool, "Sub-60 FTP test").await.unwrap();
        save_goal(&pool, "Ride a century").await.unwrap();

        let goals = load_goals(&pool).await.unwrap();
        assert_eq!(goals.len(), 2);
        // Ordered newest first (id DESC).
        assert_eq!(goals[0].description, "Ride a century");

        delete_goal(&pool, first).await.unwrap();
        let remaining = load_goals(&pool).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].description, "Ride a century");
    }
}
