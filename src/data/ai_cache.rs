//! Cached AI output.
//!
//! This shares the `settings` table with [`super::settings`] but is not
//! settings, and the distinction is worth keeping in the type system: a setting
//! is a rider's choice that needs a sensible default when unset, whereas cached
//! AI text is generated content that is simply absent until something generates
//! it, goes stale, and is safe to throw away. Nothing here has a default —
//! absence comes back as `None`.
//!
//! The morning briefing is the clearest case: it is stored alongside the date it
//! was written for precisely so a stale one can be recognised and replaced.

use anyhow::Result;
use sqlx::SqlitePool;

use super::db;
use crate::ai::brief::{DailyBrief, BRIEF_VERSION};
// A plain domain enum with no UI dependency — see `retrospective`.
use crate::ai::retrospective::RetroPeriod;

/// The keys cached AI output is stored under.
pub mod keys {
    /// The whole daily brief, as JSON.
    ///
    /// One key rather than one per section, because it is one artefact. The
    /// date it was written for and the inputs it was written from have to move
    /// with the text they describe — separate rows can be written apart and
    /// read apart, and a brief paired with someone else's date stamp is exactly
    /// the confusion this consolidation exists to remove.
    pub const DAILY_BRIEF: &str = "ai.daily_brief";

    pub const WEEKLY_RETROSPECTIVE: &str = "ai.weekly_retrospective";
    pub const MONTHLY_RETROSPECTIVE: &str = "ai.monthly_retrospective";
}

/// An empty stored string means "nothing cached", not "cached an empty answer" —
/// every consumer already treats the two the same way.
fn present(raw: Option<String>) -> Option<String> {
    raw.filter(|v| !v.is_empty())
}

// ── Daily brief ──────────────────────────────────────────────────────────────

/// The brief on hand, or `None` when there is nothing this build can use.
///
/// A stored brief that will not parse, or that carries a version this build
/// does not understand, comes back as `None` rather than as an error. Both mean
/// the same thing to every caller — there is nothing to show — and the repair
/// in both cases is to generate a new one. Failing the read instead would stop
/// the caller before it got there.
pub async fn daily_brief(pool: &SqlitePool) -> Result<Option<DailyBrief>> {
    let Some(raw) = present(db::get_setting(pool, keys::DAILY_BRIEF).await?) else {
        return Ok(None);
    };
    let brief: DailyBrief = match serde_json::from_str(&raw) {
        Ok(brief) => brief,
        Err(e) => {
            tracing::warn!("Discarding an unreadable cached brief: {e}");
            return Ok(None);
        }
    };
    if brief.version != BRIEF_VERSION {
        tracing::info!(
            "Discarding a brief written by another version (v{}, this build reads v{})",
            brief.version,
            BRIEF_VERSION
        );
        return Ok(None);
    }
    Ok(Some(brief))
}

pub async fn save_daily_brief(pool: &SqlitePool, brief: &DailyBrief) -> Result<()> {
    let json = serde_json::to_string(brief)?;
    db::set_setting(pool, keys::DAILY_BRIEF, &json).await
}

/// The workout today's brief points at, for the calendar's schedule dialog.
///
/// Only today's brief answers. The dialog offers this as a preselection, and
/// preselecting yesterday's recommendation is worse than preselecting nothing.
/// A brief that recommended nothing falls back to the session it was written
/// about, which is what the rider is actually doing today.
pub async fn brief_workout_name(pool: &SqlitePool, today: &str) -> Result<Option<String>> {
    let Some(brief) = daily_brief(pool).await? else {
        return Ok(None);
    };
    if !brief.is_for(today) {
        return Ok(None);
    }
    Ok(brief
        .recommended_workout
        .or(brief.planned_workout)
        .filter(|n| !n.trim().is_empty()))
}

// ── Retrospectives ───────────────────────────────────────────────────────────

/// Keyed by period rather than exposing the two key names, so a caller cannot
/// pair the weekly key with a monthly lookback.
fn retrospective_key(period: RetroPeriod) -> &'static str {
    match period {
        RetroPeriod::Weekly => keys::WEEKLY_RETROSPECTIVE,
        RetroPeriod::Monthly => keys::MONTHLY_RETROSPECTIVE,
    }
}

pub async fn retrospective(pool: &SqlitePool, period: RetroPeriod) -> Result<Option<String>> {
    Ok(present(
        db::get_setting(pool, retrospective_key(period)).await?,
    ))
}

pub async fn set_retrospective(pool: &SqlitePool, period: RetroPeriod, text: &str) -> Result<()> {
    db::set_setting(pool, retrospective_key(period), text).await
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_pool() -> SqlitePool {
        // One connection: a pool of them shares one in-memory database and
        // races on its schema lock. See `db::testing::empty_memory_pool`.
        let pool = crate::data::db::testing::empty_memory_pool().await;
        crate::data::migrate::run(&pool)
            .await
            .expect("migration should succeed");
        pool
    }

    // ── Daily brief ──────────────────────────────────────────────────────────

    fn brief() -> DailyBrief {
        DailyBrief {
            version: BRIEF_VERSION,
            written_for: "2026-08-11".into(),
            fingerprint: "v1|rides:143".into(),
            readiness: Some("Fresh this morning.".into()),
            form: Some("Fitness is building.".into()),
            session: Some("Ride it as written.".into()),
            fueling: Some("Pilav before.".into()),
            verdict: crate::training::program::CoachVerdict::Ease,
            planned_workout: Some("Threshold 3x12".into()),
            ..DailyBrief::default()
        }
    }

    #[tokio::test]
    async fn should_have_no_brief_before_one_is_written() {
        let pool = test_pool().await;
        assert_eq!(daily_brief(&pool).await.unwrap(), None);
    }

    #[tokio::test]
    async fn should_round_trip_a_brief_with_its_sections_verdict_and_fingerprint() {
        let pool = test_pool().await;
        save_daily_brief(&pool, &brief()).await.unwrap();
        assert_eq!(daily_brief(&pool).await.unwrap(), Some(brief()));
    }

    #[tokio::test]
    async fn should_recognise_yesterdays_brief_as_written_for_another_day() {
        let pool = test_pool().await;
        save_daily_brief(&pool, &brief()).await.unwrap();

        let cached = daily_brief(&pool).await.unwrap().expect("it was saved");
        assert!(cached.is_for("2026-08-11"));
        assert!(!cached.is_for("2026-08-12"));
    }

    #[tokio::test]
    async fn should_ignore_a_brief_written_by_another_version() {
        // Its sections may no longer mean what this build thinks they do.
        let pool = test_pool().await;
        let stale = DailyBrief {
            version: BRIEF_VERSION + 1,
            ..brief()
        };
        save_daily_brief(&pool, &stale).await.unwrap();
        assert_eq!(daily_brief(&pool).await.unwrap(), None);
    }

    #[tokio::test]
    async fn should_ignore_an_unparseable_brief_rather_than_failing_the_read() {
        // A caller that gets an error here stops; one that gets None generates
        // a replacement, which is the actual repair.
        let pool = test_pool().await;
        db::set_setting(&pool, keys::DAILY_BRIEF, "{ not json")
            .await
            .unwrap();
        assert_eq!(daily_brief(&pool).await.unwrap(), None);
    }

    #[tokio::test]
    async fn should_treat_an_empty_stored_brief_as_nothing_cached() {
        let pool = test_pool().await;
        db::set_setting(&pool, keys::DAILY_BRIEF, "").await.unwrap();
        assert_eq!(daily_brief(&pool).await.unwrap(), None);
    }

    // ── The workout the calendar dialog preselects ───────────────────────────

    #[tokio::test]
    async fn should_offer_todays_recommendation_to_the_calendar_dialog() {
        let pool = test_pool().await;
        let recommended = DailyBrief {
            recommended_workout: Some("Recovery Spin".into()),
            ..brief()
        };
        save_daily_brief(&pool, &recommended).await.unwrap();
        assert_eq!(
            brief_workout_name(&pool, "2026-08-11").await.unwrap(),
            Some("Recovery Spin".into())
        );
    }

    #[tokio::test]
    async fn should_fall_back_to_the_planned_session_when_nothing_was_recommended() {
        // With a program running the brief never recommends; the session the
        // rider is actually doing is the useful preselection.
        let pool = test_pool().await;
        save_daily_brief(&pool, &brief()).await.unwrap();
        assert_eq!(
            brief_workout_name(&pool, "2026-08-11").await.unwrap(),
            Some("Threshold 3x12".into())
        );
    }

    #[tokio::test]
    async fn should_offer_nothing_from_a_brief_written_for_another_day() {
        // Preselecting yesterday's pick is worse than preselecting nothing.
        let pool = test_pool().await;
        save_daily_brief(&pool, &brief()).await.unwrap();
        assert_eq!(brief_workout_name(&pool, "2026-08-12").await.unwrap(), None);
    }

    #[tokio::test]
    async fn should_offer_nothing_when_the_brief_names_no_workout_at_all() {
        let pool = test_pool().await;
        let nameless = DailyBrief {
            planned_workout: None,
            recommended_workout: None,
            ..brief()
        };
        save_daily_brief(&pool, &nameless).await.unwrap();
        assert_eq!(brief_workout_name(&pool, "2026-08-11").await.unwrap(), None);
    }

    // ── Retrospectives ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn should_keep_the_weekly_and_monthly_retrospectives_apart() {
        let pool = test_pool().await;
        set_retrospective(&pool, RetroPeriod::Weekly, "Seven days.")
            .await
            .unwrap();
        set_retrospective(&pool, RetroPeriod::Monthly, "Thirty days.")
            .await
            .unwrap();

        assert_eq!(
            retrospective(&pool, RetroPeriod::Weekly)
                .await
                .unwrap()
                .as_deref(),
            Some("Seven days.")
        );
        assert_eq!(
            retrospective(&pool, RetroPeriod::Monthly)
                .await
                .unwrap()
                .as_deref(),
            Some("Thirty days.")
        );
    }

    #[tokio::test]
    async fn should_have_no_retrospective_before_one_is_generated() {
        let pool = test_pool().await;
        assert_eq!(
            retrospective(&pool, RetroPeriod::Weekly).await.unwrap(),
            None
        );
    }

    // ── Keys ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn should_namespace_every_cache_key_under_ai() {
        // The prefix is what lets a future cache-clear find these rows without
        // touching a rider's settings.
        for key in [
            keys::DAILY_BRIEF,
            keys::WEEKLY_RETROSPECTIVE,
            keys::MONTHLY_RETROSPECTIVE,
        ] {
            assert!(key.starts_with("ai."), "cache key not namespaced: {key}");
        }
    }
}
