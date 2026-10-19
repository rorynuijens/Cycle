//! The local mirror of Intervals.icu activities and workouts.

use anyhow::Result;

// Lives with the sessions it reads.
use super::linked_icu_ids;
use chrono::{NaiveDate, NaiveDateTime};
use sqlx::{Row, SqlitePool};

/// Intervals.icu activities that no local session already accounts for.
///
/// Anything the app recorded itself is the better copy — it has per-second data,
/// RPE and the plan it was ridden against — so a ride that made the round trip
/// through Garmin or Strava is dropped here and read from the local session
/// instead. Every place that merges the two lists should load through this, or
/// the same ride is shown, counted and fed to the AI twice.
pub async fn load_unlinked_intervals_activities(
    pool: &SqlitePool,
) -> Result<Vec<IntervalsActivity>> {
    let linked = linked_icu_ids(pool).await?;
    let activities = load_intervals_activities(pool).await?;
    // Collapse before filtering, keeping the linked copy: a ride uploaded twice
    // has only one of its copies linked, so filtering first would leave the
    // other standing alongside the local session.
    Ok(
        crate::data::dedupe::collapse_duplicates(activities, |a| linked.contains(&a.icu_id))
            .into_iter()
            .filter(|a| !linked.contains(&a.icu_id))
            .collect(),
    )
}

/// As [`load_unlinked_intervals_activities`], for a date range.
pub async fn load_unlinked_intervals_activities_between(
    pool: &SqlitePool,
    start_date: &str,
    end_date: &str,
) -> Result<Vec<IntervalsActivity>> {
    let linked = linked_icu_ids(pool).await?;
    let activities = load_intervals_activities_between(pool, start_date, end_date).await?;
    Ok(
        crate::data::dedupe::collapse_duplicates(activities, |a| linked.contains(&a.icu_id))
            .into_iter()
            .filter(|a| !linked.contains(&a.icu_id))
            .collect(),
    )
}

/// Upsert an Intervals.icu activity (keyed on icu_id).
///
/// A sync can add to a ride or correct it, never empty it: a figure that
/// arrives missing, or a blank name or sport, leaves what is cached alone.
/// Intervals.icu strips activities it may not share down to an id and a date —
/// Strava's already, Garmin's likely next — and that copy must not wipe the
/// ride the rider already has.
#[allow(clippy::too_many_arguments)]
pub async fn upsert_intervals_activity(
    pool: &SqlitePool,
    icu_id: &str,
    date: NaiveDate,
    name: &str,
    tss: Option<f32>,
    duration_secs: Option<u32>,
    average_watts: Option<u32>,
    normalized_watts: Option<u32>,
    average_hr: Option<u32>,
    max_hr: Option<u32>,
    sport_type: &str,
    start_datetime_local: Option<NaiveDateTime>,
    distance_m: Option<f32>,
    elevation_gain_m: Option<f32>,
    average_cadence: Option<f32>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO intervals_activities
             (icu_id, date, name, tss, duration_secs,
              average_watts, normalized_watts, average_hr, max_hr, sport_type,
              start_datetime_local, distance_m, elevation_gain_m, average_cadence)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(icu_id) DO UPDATE SET
             date                 = excluded.date,
             name                 = COALESCE(NULLIF(excluded.name, ''), name),
             tss                  = COALESCE(excluded.tss, tss),
             duration_secs        = COALESCE(excluded.duration_secs, duration_secs),
             average_watts        = COALESCE(excluded.average_watts, average_watts),
             normalized_watts     = COALESCE(excluded.normalized_watts, normalized_watts),
             average_hr           = COALESCE(excluded.average_hr, average_hr),
             max_hr               = COALESCE(excluded.max_hr, max_hr),
             sport_type           = COALESCE(NULLIF(excluded.sport_type, ''), sport_type),
             start_datetime_local =
                 COALESCE(excluded.start_datetime_local, start_datetime_local),
             distance_m           = COALESCE(excluded.distance_m, distance_m),
             elevation_gain_m     = COALESCE(excluded.elevation_gain_m, elevation_gain_m),
             average_cadence      = COALESCE(excluded.average_cadence, average_cadence)",
    )
    .bind(icu_id)
    .bind(date.format("%Y-%m-%d").to_string())
    .bind(name)
    .bind(tss.map(|v| v as f64))
    .bind(duration_secs.map(|v| v as i64))
    .bind(average_watts.map(|v| v as f64))
    .bind(normalized_watts.map(|v| v as f64))
    .bind(average_hr.map(|v| v as f64))
    .bind(max_hr.map(|v| v as i64))
    .bind(sport_type)
    .bind(start_datetime_local.map(|dt| dt.format("%Y-%m-%dT%H:%M:%S").to_string()))
    .bind(distance_m.map(|v| v as f64))
    .bind(elevation_gain_m.map(|v| v as f64))
    .bind(average_cadence.map(|v| v as f64))
    .execute(pool)
    .await?;
    Ok(())
}

/// Load (date, tss) pairs from Intervals.icu activities for CTL/ATL calculation.
pub async fn load_intervals_tss_pairs(pool: &SqlitePool) -> Result<Vec<(NaiveDate, f32)>> {
    // Repeated uploads of one ride are collapsed first. Each copy carries the
    // same TSS, so counting them all would credit the rider two or three times
    // for a single session and inflate fitness and fatigue for weeks after.
    //
    // Session-linked activities are deliberately kept: the local ride excludes
    // itself when it has a link, and this is the row that stands in for it.
    let activities = load_intervals_activities(pool).await?;
    Ok(
        crate::data::dedupe::collapse_duplicates(activities, |_| false)
            .into_iter()
            .filter_map(|a| a.tss.map(|tss| (a.date, tss)))
            .collect(),
    )
}

/// A full Intervals.icu activity row, used for display in History and AI coaching.
#[derive(Debug, Clone)]
pub struct IntervalsActivity {
    pub icu_id: String,
    pub date: NaiveDate,
    pub name: String,
    pub tss: Option<f32>,
    pub duration_secs: Option<u32>,
    pub average_watts: Option<u32>,
    pub normalized_watts: Option<u32>,
    pub average_hr: Option<u32>,
    pub max_hr: Option<u32>,
    pub sport_type: String,
    pub start_datetime_local: Option<NaiveDateTime>,
    pub distance_m: Option<f32>,
    pub elevation_gain_m: Option<f32>,
    pub average_cadence: Option<f32>,
}

/// Total count of cached Intervals.icu activities.
pub async fn count_intervals_activities(pool: &SqlitePool) -> Result<i64> {
    // Counts rides, not rows: a ride uploaded twice is still one session, and
    // this figure goes to the AI coach as the athlete's training history.
    let activities = load_intervals_activities(pool).await?;
    Ok(crate::data::dedupe::collapse_duplicates(activities, |_| false).len() as i64)
}

/// Delete a single Intervals.icu activity by its icu_id.
pub async fn delete_intervals_activity(pool: &SqlitePool, icu_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM intervals_activities WHERE icu_id = ?")
        .bind(icu_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Load all Intervals.icu activities, newest first.
pub async fn load_intervals_activities(pool: &SqlitePool) -> Result<Vec<IntervalsActivity>> {
    let rows = sqlx::query(
        "SELECT icu_id, date, name, tss, duration_secs,
                average_watts, normalized_watts, average_hr, max_hr, sport_type,
                start_datetime_local, distance_m, elevation_gain_m, average_cadence
         FROM intervals_activities ORDER BY date DESC",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let date_str: &str = r.get("date");
            let date = NaiveDate::parse_from_str(date_str, "%Y-%m-%d").ok()?;
            let start_datetime_local = r
                .get::<Option<&str>, _>("start_datetime_local")
                .and_then(|s| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").ok());
            Some(IntervalsActivity {
                icu_id: r.get("icu_id"),
                date,
                name: r.get("name"),
                tss: r.get::<Option<f64>, _>("tss").map(|v| v as f32),
                duration_secs: super::stored_count(r.get("duration_secs")),
                average_watts: r.get::<Option<f64>, _>("average_watts").map(|v| v as u32),
                normalized_watts: r
                    .get::<Option<f64>, _>("normalized_watts")
                    .map(|v| v as u32),
                average_hr: r.get::<Option<f64>, _>("average_hr").map(|v| v as u32),
                max_hr: super::stored_count(r.get("max_hr")),
                sport_type: r
                    .get::<Option<&str>, _>("sport_type")
                    .unwrap_or("")
                    .to_string(),
                start_datetime_local,
                distance_m: r.get::<Option<f64>, _>("distance_m").map(|v| v as f32),
                elevation_gain_m: r
                    .get::<Option<f64>, _>("elevation_gain_m")
                    .map(|v| v as f32),
                average_cadence: r.get::<Option<f64>, _>("average_cadence").map(|v| v as f32),
            })
        })
        .collect())
}

/// One row of `intervals_workouts`. The columns are carried whole so callers
/// can pick what they need; not every one has a reader today.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct IntervalsWorkout {
    pub id: i64,
    pub icu_id: String,
    pub name: String,
    pub description: String,
    pub duration_secs: Option<u32>,
    pub tss: Option<f32>,
}

/// Upsert an Intervals.icu workout template (keyed on icu_id).
pub async fn upsert_intervals_workout(
    pool: &SqlitePool,
    icu_id: &str,
    name: &str,
    description: &str,
    duration_secs: Option<u32>,
    tss: Option<f32>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO intervals_workouts (icu_id, name, description, duration_secs, tss)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(icu_id) DO UPDATE SET
             name = excluded.name,
             description = excluded.description,
             duration_secs = excluded.duration_secs,
             tss = excluded.tss",
    )
    .bind(icu_id)
    .bind(name)
    .bind(description)
    .bind(duration_secs.map(|v| v as i64))
    .bind(tss.map(|v| v as f64))
    .execute(pool)
    .await?;
    Ok(())
}

/// Load all Intervals.icu workout templates, sorted by name.
pub async fn load_intervals_workouts(pool: &SqlitePool) -> Result<Vec<IntervalsWorkout>> {
    let rows = sqlx::query(
        "SELECT id, icu_id, name, description, duration_secs, tss
         FROM intervals_workouts ORDER BY name",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| IntervalsWorkout {
            id: r.get("id"),
            icu_id: r.get("icu_id"),
            name: r.get("name"),
            description: r.get("description"),
            duration_secs: super::stored_count(r.get("duration_secs")),
            tss: r.get::<Option<f64>, _>("tss").map(|v| v as f32),
        })
        .collect())
}

/// Delete all cached Intervals.icu workouts (called before a fresh sync).
pub async fn clear_intervals_workouts(pool: &SqlitePool) -> Result<()> {
    sqlx::query("DELETE FROM intervals_workouts")
        .execute(pool)
        .await?;
    Ok(())
}

/// Count of cached Intervals.icu workout templates.
pub async fn count_intervals_workouts(pool: &SqlitePool) -> Result<i64> {
    let row = sqlx::query("SELECT COUNT(*) AS cnt FROM intervals_workouts")
        .fetch_one(pool)
        .await?;
    Ok(row.get("cnt"))
}

/// Load Intervals.icu activities whose date falls within [start_date, end_date] (ISO 8601).
pub async fn load_intervals_activities_between(
    pool: &SqlitePool,
    start_date: &str,
    end_date: &str,
) -> Result<Vec<IntervalsActivity>> {
    let rows = sqlx::query(
        "SELECT icu_id, date, name, tss, duration_secs,
                average_watts, normalized_watts, average_hr, max_hr, sport_type,
                start_datetime_local, distance_m, elevation_gain_m, average_cadence
         FROM intervals_activities WHERE date >= ? AND date <= ? ORDER BY date ASC",
    )
    .bind(start_date)
    .bind(end_date)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let date_str: &str = r.get("date");
            let date = NaiveDate::parse_from_str(date_str, "%Y-%m-%d").ok()?;
            let start_datetime_local = r
                .get::<Option<&str>, _>("start_datetime_local")
                .and_then(|s| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").ok());
            Some(IntervalsActivity {
                icu_id: r.get("icu_id"),
                date,
                name: r.get("name"),
                tss: r.get::<Option<f64>, _>("tss").map(|v| v as f32),
                duration_secs: super::stored_count(r.get("duration_secs")),
                average_watts: r.get::<Option<f64>, _>("average_watts").map(|v| v as u32),
                normalized_watts: r
                    .get::<Option<f64>, _>("normalized_watts")
                    .map(|v| v as u32),
                average_hr: r.get::<Option<f64>, _>("average_hr").map(|v| v as u32),
                max_hr: super::stored_count(r.get("max_hr")),
                sport_type: r
                    .get::<Option<&str>, _>("sport_type")
                    .unwrap_or("")
                    .to_string(),
                start_datetime_local,
                distance_m: r.get::<Option<f64>, _>("distance_m").map(|v| v as f32),
                elevation_gain_m: r
                    .get::<Option<f64>, _>("elevation_gain_m")
                    .map(|v| v as f32),
                average_cadence: r.get::<Option<f64>, _>("average_cadence").map(|v| v as f32),
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::db::testing::test_pool;
    use crate::data::session::Session;

    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, d).expect("hardcoded valid date")
    }

    /// One ride, as Intervals.icu would send it. Two calls with the same
    /// `start` and `duration` are two uploads of the same ride.
    async fn ride(pool: &SqlitePool, icu_id: &str, d: u32, hour: u32, tss: Option<f32>) {
        upsert_intervals_activity(
            pool,
            icu_id,
            day(d),
            "Morning Ride",
            tss,
            Some(3600),
            Some(180),
            Some(195),
            Some(140),
            Some(171),
            "Ride",
            day(d).and_hms_opt(hour, 0, 0),
            Some(30_000.0),
            Some(420.0),
            Some(88.0),
        )
        .await
        .unwrap();
    }

    /// A local session linked to `icu_id`, as the round-trip matcher leaves it.
    async fn link(pool: &SqlitePool, icu_id: &str) {
        let id = crate::data::db::save_session(pool, &Session::new(None))
            .await
            .unwrap();
        crate::data::db::set_session_icu_id(pool, id, Some(icu_id))
            .await
            .unwrap();
    }

    fn ids(list: &[IntervalsActivity]) -> Vec<&str> {
        list.iter().map(|a| a.icu_id.as_str()).collect()
    }

    // ── Writing ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn should_read_back_every_field_it_wrote() {
        let pool = test_pool().await;
        upsert_intervals_activity(
            &pool,
            "i101",
            day(9),
            "Çamlıca tırmanışı",
            Some(87.5),
            Some(5340),
            Some(183),
            Some(201),
            Some(146),
            Some(178),
            "VirtualRide",
            day(9).and_hms_opt(7, 15, 30),
            Some(41_250.5),
            Some(612.0),
            Some(91.5),
        )
        .await
        .unwrap();

        let a = &load_intervals_activities(&pool).await.unwrap()[0];
        assert_eq!(a.icu_id, "i101");
        assert_eq!(a.date, day(9));
        assert_eq!(a.name, "Çamlıca tırmanışı");
        assert_eq!(a.tss, Some(87.5));
        assert_eq!(a.duration_secs, Some(5340));
        assert_eq!(a.average_watts, Some(183));
        assert_eq!(a.normalized_watts, Some(201));
        assert_eq!(a.average_hr, Some(146));
        assert_eq!(a.max_hr, Some(178));
        assert_eq!(a.sport_type, "VirtualRide");
        assert_eq!(a.start_datetime_local, day(9).and_hms_opt(7, 15, 30));
        assert_eq!(a.distance_m, Some(41_250.5));
        assert_eq!(a.elevation_gain_m, Some(612.0));
        assert_eq!(a.average_cadence, Some(91.5));
    }

    #[tokio::test]
    async fn should_update_a_ride_in_place_when_it_syncs_again() {
        let pool = test_pool().await;
        ride(&pool, "i1", 9, 7, Some(80.0)).await;
        // Intervals.icu recomputed the load after an FTP change.
        ride(&pool, "i1", 9, 7, Some(64.0)).await;
        let all = load_intervals_activities(&pool).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].tss, Some(64.0));
    }

    #[tokio::test]
    async fn should_keep_a_load_figure_a_later_sync_leaves_out() {
        let pool = test_pool().await;
        ride(&pool, "i1", 9, 7, Some(80.0)).await;
        ride(&pool, "i1", 9, 7, None).await;
        assert_eq!(
            load_intervals_activities(&pool).await.unwrap()[0].tss,
            Some(80.0)
        );
    }

    /// What Intervals.icu sends for an activity it may not share — it already
    /// does this for Strava rides: the id and the date, every figure gone.
    #[tokio::test]
    async fn should_keep_every_figure_when_a_stripped_copy_of_the_ride_arrives() {
        let pool = test_pool().await;
        ride(&pool, "i1", 9, 7, Some(80.0)).await;
        upsert_intervals_activity(
            &pool,
            "i1",
            day(9),
            "",
            None,
            None,
            None,
            None,
            None,
            None,
            "",
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        let a = &load_intervals_activities(&pool).await.unwrap()[0];
        assert_eq!(a.name, "Morning Ride");
        assert_eq!(a.sport_type, "Ride");
        assert_eq!(a.tss, Some(80.0));
        assert_eq!(a.duration_secs, Some(3600));
        assert_eq!(a.average_watts, Some(180));
        assert_eq!(a.normalized_watts, Some(195));
        assert_eq!(a.average_hr, Some(140));
        assert_eq!(a.max_hr, Some(171));
        assert_eq!(a.start_datetime_local, day(9).and_hms_opt(7, 0, 0));
        assert_eq!(a.distance_m, Some(30_000.0));
        assert_eq!(a.elevation_gain_m, Some(420.0));
        assert_eq!(a.average_cadence, Some(88.0));
    }

    #[tokio::test]
    async fn should_still_take_a_new_name_and_a_new_figure() {
        let pool = test_pool().await;
        ride(&pool, "i1", 9, 7, Some(80.0)).await;
        upsert_intervals_activity(
            &pool,
            "i1",
            day(9),
            "Indoor Cycling",
            Some(0.0),
            Some(3600),
            Some(180),
            Some(195),
            Some(140),
            Some(171),
            "VirtualRide",
            day(9).and_hms_opt(7, 0, 0),
            Some(30_000.0),
            Some(420.0),
            Some(88.0),
        )
        .await
        .unwrap();

        let a = &load_intervals_activities(&pool).await.unwrap()[0];
        assert_eq!(a.name, "Indoor Cycling");
        assert_eq!(a.sport_type, "VirtualRide");
        // Zero is a figure, not a gap.
        assert_eq!(a.tss, Some(0.0));
    }

    #[tokio::test]
    async fn should_delete_only_the_ride_asked_for() {
        let pool = test_pool().await;
        ride(&pool, "i1", 9, 7, Some(50.0)).await;
        ride(&pool, "i2", 10, 7, Some(60.0)).await;
        delete_intervals_activity(&pool, "i1").await.unwrap();
        assert_eq!(
            ids(&load_intervals_activities(&pool).await.unwrap()),
            ["i2"]
        );
    }

    // ── Reading ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn should_list_everything_newest_first() {
        let pool = test_pool().await;
        ride(&pool, "old", 1, 7, None).await;
        ride(&pool, "new", 20, 7, None).await;
        ride(&pool, "mid", 10, 7, None).await;
        assert_eq!(
            ids(&load_intervals_activities(&pool).await.unwrap()),
            ["new", "mid", "old"]
        );
    }

    #[tokio::test]
    async fn should_include_both_ends_of_a_date_range_and_nothing_outside_it() {
        let pool = test_pool().await;
        for (id, d) in [
            ("before", 4),
            ("first", 5),
            ("inside", 8),
            ("last", 11),
            ("after", 12),
        ] {
            ride(&pool, id, d, 7, None).await;
        }
        let found = load_intervals_activities_between(&pool, "2026-10-05", "2026-10-11")
            .await
            .unwrap();
        // Oldest first: the calendar lays a range out left to right.
        assert_eq!(ids(&found), ["first", "inside", "last"]);
    }

    #[tokio::test]
    async fn should_skip_a_row_with_a_corrupt_date_rather_than_fail_the_list() {
        // CLAUDE.md §5.2: a damaged row must cost that row, not the calendar.
        let pool = test_pool().await;
        ride(&pool, "good", 9, 7, None).await;
        sqlx::query(
            "INSERT INTO intervals_activities (icu_id, date, name, sport_type)
             VALUES ('bad', '2026-13-45', 'Corrupt', 'Ride')",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            ids(&load_intervals_activities(&pool).await.unwrap()),
            ["good"]
        );
    }

    #[tokio::test]
    async fn should_keep_a_ride_whose_start_time_is_unreadable() {
        // The start time only pairs duplicates; losing it must not lose the ride.
        let pool = test_pool().await;
        ride(&pool, "i1", 9, 7, Some(50.0)).await;
        sqlx::query("UPDATE intervals_activities SET start_datetime_local = 'yesterday'")
            .execute(&pool)
            .await
            .unwrap();
        let all = load_intervals_activities(&pool).await.unwrap();
        assert_eq!(ids(&all), ["i1"]);
        assert_eq!(all[0].start_datetime_local, None);
    }

    // ── Rides uploaded twice, and rides recorded here ────────────────────────

    #[tokio::test]
    async fn should_count_a_ride_uploaded_twice_once_for_fitness() {
        // Same start and length, two icu ids: a re-exported FIT file. Counting
        // both would credit the rider twice for one session.
        let pool = test_pool().await;
        ride(&pool, "upload-1", 9, 7, Some(80.0)).await;
        ride(&pool, "upload-2", 9, 7, Some(80.0)).await;
        ride(&pool, "other", 9, 17, Some(30.0)).await;
        let mut pairs = load_intervals_tss_pairs(&pool).await.unwrap();
        pairs.sort_by(|a, b| a.1.total_cmp(&b.1));
        assert_eq!(pairs, vec![(day(9), 30.0), (day(9), 80.0)]);
        assert_eq!(count_intervals_activities(&pool).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn should_still_count_a_ride_recorded_here_for_fitness() {
        // The local session steps aside when it has a link, so this row is the
        // ride's only contribution — dropping it would lose the ride.
        let pool = test_pool().await;
        ride(&pool, "i1", 9, 7, Some(80.0)).await;
        link(&pool, "i1").await;
        assert_eq!(
            load_intervals_tss_pairs(&pool).await.unwrap(),
            vec![(day(9), 80.0)]
        );
    }

    #[tokio::test]
    async fn should_leave_out_a_ride_recorded_here() {
        let pool = test_pool().await;
        ride(&pool, "mine", 9, 7, Some(80.0)).await;
        ride(&pool, "outdoor", 10, 7, Some(120.0)).await;
        link(&pool, "mine").await;
        assert_eq!(
            ids(&load_unlinked_intervals_activities(&pool).await.unwrap()),
            ["outdoor"]
        );
    }

    #[tokio::test]
    async fn should_leave_out_every_copy_of_a_ride_recorded_here() {
        // Uploaded twice, and only the second copy linked. Filtering before
        // collapsing would leave the first copy standing beside the local
        // session, and the calendar would show the ride twice.
        let pool = test_pool().await;
        ride(&pool, "copy-1", 9, 7, Some(80.0)).await;
        ride(&pool, "copy-2", 9, 7, Some(80.0)).await;
        link(&pool, "copy-2").await;
        assert!(load_unlinked_intervals_activities(&pool)
            .await
            .unwrap()
            .is_empty());
        assert!(
            load_unlinked_intervals_activities_between(&pool, "2026-10-01", "2026-10-31")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn should_keep_one_copy_of_a_ride_uploaded_twice_and_not_recorded_here() {
        let pool = test_pool().await;
        ride(&pool, "copy-1", 9, 7, Some(80.0)).await;
        ride(&pool, "copy-2", 9, 7, Some(80.0)).await;
        assert_eq!(
            load_unlinked_intervals_activities_between(&pool, "2026-10-09", "2026-10-09")
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn should_keep_two_rides_on_the_same_day_apart() {
        let pool = test_pool().await;
        ride(&pool, "morning", 9, 7, Some(40.0)).await;
        ride(&pool, "evening", 9, 18, Some(50.0)).await;
        assert_eq!(
            load_unlinked_intervals_activities(&pool)
                .await
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn should_not_read_a_negative_stored_value_as_four_billion() {
        // CLAUDE.md §5.2. A negative integer cast to u32 wraps: −1 seconds
        // would read as a 136-year ride, and the AI coach is fed this history.
        let pool = test_pool().await;
        ride(&pool, "i1", 9, 7, Some(80.0)).await;
        sqlx::query("UPDATE intervals_activities SET duration_secs = -1, max_hr = -5")
            .execute(&pool)
            .await
            .unwrap();
        let a = &load_intervals_activities(&pool).await.unwrap()[0];
        assert_eq!(a.duration_secs, None);
        assert_eq!(a.max_hr, None);
        let b = &load_intervals_activities_between(&pool, "2026-10-09", "2026-10-09")
            .await
            .unwrap()[0];
        assert_eq!(b.duration_secs, None);
        assert_eq!(b.max_hr, None);
    }

    // ── Workout templates ────────────────────────────────────────────────────

    #[tokio::test]
    async fn should_list_workout_templates_by_name_and_replace_on_resync() {
        let pool = test_pool().await;
        upsert_intervals_workout(&pool, "w2", "Sweet Spot", "3x12", Some(3600), Some(70.0))
            .await
            .unwrap();
        upsert_intervals_workout(&pool, "w1", "Endurance", "Z2", Some(5400), None)
            .await
            .unwrap();
        upsert_intervals_workout(&pool, "w2", "Sweet Spot", "3x15", Some(3900), Some(78.0))
            .await
            .unwrap();

        let all = load_intervals_workouts(&pool).await.unwrap();
        let names: Vec<&str> = all.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, ["Endurance", "Sweet Spot"]);
        assert_eq!(all[1].description, "3x15");
        assert_eq!(all[1].duration_secs, Some(3900));
        assert_eq!(all[1].tss, Some(78.0));
        assert_eq!(count_intervals_workouts(&pool).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn should_empty_the_templates_before_a_fresh_sync() {
        let pool = test_pool().await;
        upsert_intervals_workout(&pool, "w1", "Endurance", "", None, None)
            .await
            .unwrap();
        clear_intervals_workouts(&pool).await.unwrap();
        assert_eq!(count_intervals_workouts(&pool).await.unwrap(), 0);
        assert!(load_intervals_workouts(&pool).await.unwrap().is_empty());
    }
}
