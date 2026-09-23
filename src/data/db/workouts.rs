//! The workout library, and building a workout from a ride that was already done.

use crate::data::session::Session;
use crate::data::workout::{Segment, Workout, WorkoutCategory};
use anyhow::Result;
use sqlx::{Row, SqlitePool};

/// Insert a workout and return its new DB id.
pub async fn save_workout(pool: &SqlitePool, workout: &Workout) -> Result<i64> {
    let segments_json = serde_json::to_string(&workout.segments)?;
    let result = sqlx::query(
        "INSERT INTO workouts (name, description, duration_secs, tss, category, segments_json)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&workout.name)
    .bind(&workout.description)
    .bind(workout.duration_secs as i64)
    .bind(workout.tss as f64)
    .bind(workout.category.as_db_str())
    .bind(&segments_json)
    .execute(pool)
    .await?;
    Ok(result.last_insert_rowid())
}

pub async fn load_workouts(pool: &SqlitePool) -> Result<Vec<Workout>> {
    let rows = sqlx::query(
        "SELECT id, name, description, duration_secs, tss, category, segments_json
         FROM workouts ORDER BY id",
    )
    .fetch_all(pool)
    .await?;

    let mut workouts = Vec::new();
    for r in rows {
        let segments: Vec<Segment> = serde_json::from_str(r.get("segments_json"))?;
        let category_str: String = r.get("category");
        workouts.push(Workout {
            id: r.get("id"),
            name: r.get("name"),
            description: r.get("description"),
            duration_secs: r.get::<i64, _>("duration_secs") as u32,
            tss: r.get::<f64, _>("tss") as f32,
            category: WorkoutCategory::from_db_str(&category_str),
            segments,
        });
    }
    Ok(workouts)
}

/// Load a single workout by its primary-key id. Returns `None` if not found.
pub async fn load_workout_by_id(pool: &SqlitePool, id: i64) -> Result<Option<Workout>> {
    let row = sqlx::query(
        "SELECT id, name, description, duration_secs, tss, category, segments_json
         FROM workouts WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    match row {
        None => Ok(None),
        Some(r) => {
            let segments: Vec<Segment> = serde_json::from_str(r.get("segments_json"))?;
            let category_str: String = r.get("category");
            Ok(Some(Workout {
                id: r.get("id"),
                name: r.get("name"),
                description: r.get("description"),
                duration_secs: r.get::<i64, _>("duration_secs") as u32,
                tss: r.get::<f64, _>("tss") as f32,
                category: WorkoutCategory::from_db_str(&category_str),
                segments,
            }))
        }
    }
}

/// Update an existing workout (name, description, category, segments).
pub async fn update_workout(pool: &SqlitePool, workout: &Workout) -> Result<()> {
    let segments_json = serde_json::to_string(&workout.segments)?;
    sqlx::query(
        "UPDATE workouts
         SET name=?, description=?, duration_secs=?, tss=?, category=?, segments_json=?
         WHERE id=?",
    )
    .bind(&workout.name)
    .bind(&workout.description)
    .bind(workout.duration_secs as i64)
    .bind(workout.tss as f64)
    .bind(workout.category.as_db_str())
    .bind(&segments_json)
    .bind(workout.id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Delete a workout by id. Only call for custom (user-created) workouts.
pub async fn delete_workout(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query("DELETE FROM workouts WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Seed the workouts table with the full training library if not already seeded.
pub async fn seed_workouts(pool: &SqlitePool) -> Result<()> {
    let already_seeded: bool =
        sqlx::query("SELECT 1 FROM workouts WHERE name = 'Active Recovery 30' LIMIT 1")
            .fetch_optional(pool)
            .await?
            .is_some();

    if already_seeded {
        // Additive migration: insert any library workouts that were added after the
        // initial seed but are not yet present (matched by exact name).
        let new_workouts: &[&str] = &["Ramp Test", "20-Minute FTP Test"];
        for &name in new_workouts {
            let exists = sqlx::query("SELECT 1 FROM workouts WHERE name = ? LIMIT 1")
                .bind(name)
                .fetch_optional(pool)
                .await?
                .is_some();
            if !exists {
                if let Some(workout) = Workout::workout_library()
                    .into_iter()
                    .find(|w| w.name == name)
                {
                    save_workout(pool, &workout).await?;
                    tracing::info!("Migrated new workout: {name}");
                }
            }
        }
        refresh_ramp_test(pool).await?;
        return Ok(());
    }

    let library = Workout::workout_library();
    let count = library.len();
    for workout in library {
        save_workout(pool, &workout).await?;
    }
    tracing::info!("Seeded {count} workouts");
    Ok(())
}

/// Bring an already-seeded Ramp Test row up to the current ladder.
///
/// Unlike every other library workout, the ramp test is a *protocol*: its ladder
/// is what produces the FTP number, so a row seeded from an older, shorter
/// ladder would cap a strong rider's result at the top step it happens to have.
/// The row is updated in place rather than replaced, because calendar entries
/// and recorded rides refer to it by id.
///
/// Matched on segment count, which is what actually changes when the ladder
/// does, and so a no-op on every launch after the first. A row the rider has
/// edited themselves is overwritten — this is a seeded workout, and the editor
/// offers Duplicate for keeping a variant.
async fn refresh_ramp_test(pool: &SqlitePool) -> Result<()> {
    let current = Workout::ramp_test();
    let row = sqlx::query("SELECT id, segments_json FROM workouts WHERE name = ? LIMIT 1")
        .bind(crate::data::workout::RAMP_TEST_NAME)
        .fetch_optional(pool)
        .await?;
    let Some(row) = row else { return Ok(()) };

    let stored: Vec<Segment> = serde_json::from_str(row.get("segments_json"))?;
    if stored.len() == current.segments.len() {
        return Ok(());
    }

    let updated = Workout {
        id: row.get("id"),
        ..current
    };
    update_workout(pool, &updated).await?;
    tracing::info!(
        "Refreshed the Ramp Test ladder: {} segments -> {}",
        stored.len(),
        updated.segments.len()
    );
    Ok(())
}

fn build_activity_workout(
    name: &str,
    description: &str,
    duration_secs: u32,
    avg_pct: f32,
) -> crate::data::workout::Workout {
    use crate::data::workout::{Segment, WorkoutCategory};

    let wu_secs = (duration_secs as f32 * 0.10) as u32;
    let main_secs = (duration_secs as f32 * 0.80) as u32;
    let cd_secs = duration_secs.saturating_sub(wu_secs + main_secs);

    let segments = vec![
        Segment {
            duration_secs: wu_secs,
            power_low_pct: 50.0,
            power_high_pct: 50.0,
            label: None,
            cadence_target: None,
        },
        Segment {
            duration_secs: main_secs,
            power_low_pct: avg_pct,
            power_high_pct: avg_pct,
            label: Some("Main Effort".to_string()),
            cadence_target: None,
        },
        Segment {
            duration_secs: cd_secs,
            power_low_pct: 50.0,
            power_high_pct: 50.0,
            label: None,
            cadence_target: None,
        },
    ];

    let if_val = avg_pct / 100.0;
    let tss = (duration_secs as f32 / 3600.0) * (if_val * if_val) * 100.0;

    crate::data::workout::Workout {
        id: 0,
        name: name.to_string(),
        description: description.to_string(),
        duration_secs,
        tss,
        category: WorkoutCategory::Custom,
        segments,
    }
}

/// Create a simple workout record derived from a past session (for scheduling) and return its id.
pub async fn create_workout_from_session(
    pool: &SqlitePool,
    session: &Session,
    workout_name: &str,
    ftp: u32,
) -> Result<i64> {
    let duration_secs = session.duration_secs() as u32;
    let avg_pct = if ftp > 0 {
        session
            .average_power()
            .map(|p| ((p / ftp as f32) * 100.0).clamp(40.0, 200.0))
            .unwrap_or(70.0)
    } else {
        70.0
    };
    let w = build_activity_workout(
        workout_name,
        "Auto-generated from a past session",
        duration_secs,
        avg_pct,
    );
    save_workout(pool, &w).await
}

/// Create a simple workout from an Intervals.icu activity summary.
pub async fn create_workout_from_icu_activity(
    pool: &SqlitePool,
    name: &str,
    duration_secs: u32,
    avg_watts: Option<u32>,
    ftp: u32,
) -> Result<i64> {
    let avg_pct = if ftp > 0 {
        avg_watts
            .map(|w| ((w as f32 / ftp as f32) * 100.0).clamp(40.0, 200.0))
            .unwrap_or(70.0)
    } else {
        70.0
    };
    let w = build_activity_workout(
        name,
        "Auto-generated from an Intervals.icu activity",
        duration_secs,
        avg_pct,
    );
    save_workout(pool, &w).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::db::testing::*;

    /// The Ramp Test as it shipped in 0.10.0: a twelve-step ladder topping out
    /// at 148 % of FTP.
    fn old_ramp_test() -> Workout {
        let mut segments = vec![Segment::ramp(600, 40.0, 60.0, "Warm-up")];
        for step in 0..12 {
            let pct = 60.0 + 8.0 * step as f32;
            segments.push(Segment::steady(60, pct, "Ramp"));
        }
        segments.push(Segment::steady(600, 40.0, "Cool-down"));
        Workout::from_segments(
            crate::data::workout::RAMP_TEST_NAME,
            "the old description",
            WorkoutCategory::Custom,
            segments,
        )
    }

    #[tokio::test]
    async fn should_bring_an_old_ramp_test_row_up_to_the_current_ladder() {
        // The ladder is the protocol: a row seeded from the shorter one caps a
        // strong rider's result at the top step it happens to have.
        let pool = test_pool().await;
        let id = save_workout(&pool, &old_ramp_test()).await.unwrap();

        refresh_ramp_test(&pool).await.unwrap();

        let loaded = load_workout_by_id(&pool, id)
            .await
            .unwrap()
            .expect("the row is updated in place, not replaced");
        assert_eq!(
            loaded.segments.len(),
            Workout::ramp_test().segments.len(),
            "the ladder was not refreshed"
        );
        let top = loaded
            .segments
            .iter()
            .map(|s| s.power_high_pct)
            .fold(0.0_f32, f32::max);
        assert!(top > 148.0, "still topping out at {top}%");
        assert_eq!(loaded.id, id, "calendar entries refer to this row by id");
        assert!(
            loaded.is_ramp_test(),
            "and it is still recognised as the test"
        );
    }

    #[tokio::test]
    async fn should_leave_a_current_ramp_test_row_alone() {
        // Runs on every launch, so it has to be a no-op once it has run.
        let pool = test_pool().await;
        let id = save_workout(&pool, &Workout::ramp_test()).await.unwrap();
        let before = load_workout_by_id(&pool, id).await.unwrap().unwrap();

        refresh_ramp_test(&pool).await.unwrap();

        let after = load_workout_by_id(&pool, id).await.unwrap().unwrap();
        assert_eq!(after.segments.len(), before.segments.len());
        assert_eq!(after.description, before.description);
    }

    #[tokio::test]
    async fn should_do_nothing_when_there_is_no_ramp_test_to_refresh() {
        // A rider who deleted it has deleted it; re-seeding here would resurrect
        // a workout they threw away.
        let pool = test_pool().await;
        refresh_ramp_test(&pool).await.unwrap();
        assert!(load_workouts(&pool).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn save_and_load_workout_by_id_preserves_segments() {
        let pool = test_pool().await;
        let id = save_workout(&pool, &sample_workout()).await.unwrap();
        assert!(id > 0);

        let loaded = load_workout_by_id(&pool, id)
            .await
            .unwrap()
            .expect("workout should exist");
        assert_eq!(loaded.name, "Test Intervals");
        assert_eq!(loaded.category, WorkoutCategory::Threshold);
        assert_eq!(loaded.segments.len(), 2);
        assert_eq!(loaded.segments[1].power_low_pct, 100.0);
    }

    #[tokio::test]
    async fn load_workout_by_id_returns_none_for_missing_id() {
        let pool = test_pool().await;
        assert!(load_workout_by_id(&pool, 9999).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_workout_removes_the_row() {
        let pool = test_pool().await;
        let id = save_workout(&pool, &sample_workout()).await.unwrap();
        delete_workout(&pool, id).await.unwrap();
        assert!(load_workout_by_id(&pool, id).await.unwrap().is_none());
    }
}
