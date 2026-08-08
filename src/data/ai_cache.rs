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
// A plain domain enum with no UI dependency — see `retrospective`.
use crate::ai::retrospective::RetroPeriod;

/// The keys cached AI output is stored under.
pub mod keys {
    pub const SUGGESTION_RESPONSE: &str = "ai.suggestion_response";
    pub const SUGGESTION_WORKOUT_NAME: &str = "ai.suggestion_workout_name";
    pub const SUGGESTION_WORKOUT_DETAIL: &str = "ai.suggestion_workout_detail";

    pub const FITNESS_INSIGHT: &str = "ai.fitness_insight";

    pub const BRIEFING_TEXT: &str = "ai.morning_briefing_text";
    pub const BRIEFING_DATE: &str = "ai.morning_briefing_date";

    pub const WEEKLY_RETROSPECTIVE: &str = "ai.weekly_retrospective";
    pub const MONTHLY_RETROSPECTIVE: &str = "ai.monthly_retrospective";
}

/// An empty stored string means "nothing cached", not "cached an empty answer" —
/// every consumer already treats the two the same way.
fn present(raw: Option<String>) -> Option<String> {
    raw.filter(|v| !v.is_empty())
}

// ── Ride suggestion ──────────────────────────────────────────────────────────

/// The most recent "what should I ride today?" answer, cached so the page has
/// something to show before the next request comes back.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CachedSuggestion {
    /// The coach's prose answer.
    pub response: String,
    /// The workout it recommended, if it named one.
    pub workout_name: String,
    /// The longer description of that workout.
    pub workout_detail: String,
}

/// Read all three parts together — the coaching page shows them as one thing.
/// Missing parts come back empty rather than failing.
pub async fn load_suggestion(pool: &SqlitePool) -> Result<CachedSuggestion> {
    Ok(CachedSuggestion {
        response: db::get_setting(pool, keys::SUGGESTION_RESPONSE)
            .await?
            .unwrap_or_default(),
        workout_name: db::get_setting(pool, keys::SUGGESTION_WORKOUT_NAME)
            .await?
            .unwrap_or_default(),
        workout_detail: db::get_setting(pool, keys::SUGGESTION_WORKOUT_DETAIL)
            .await?
            .unwrap_or_default(),
    })
}

pub async fn save_suggestion(pool: &SqlitePool, suggestion: &CachedSuggestion) -> Result<()> {
    db::set_setting(pool, keys::SUGGESTION_RESPONSE, &suggestion.response).await?;
    db::set_setting(
        pool,
        keys::SUGGESTION_WORKOUT_NAME,
        &suggestion.workout_name,
    )
    .await?;
    db::set_setting(
        pool,
        keys::SUGGESTION_WORKOUT_DETAIL,
        &suggestion.workout_detail,
    )
    .await
}

/// Just the recommended workout's name — the dashboard and the calendar's
/// schedule dialog want this on its own, without the surrounding prose.
pub async fn suggestion_workout_name(pool: &SqlitePool) -> Result<Option<String>> {
    Ok(present(
        db::get_setting(pool, keys::SUGGESTION_WORKOUT_NAME).await?,
    ))
}

/// Written on its own when the morning briefing recommends a workout, which
/// happens independently of a full suggestion being generated.
pub async fn set_suggestion_workout_name(pool: &SqlitePool, name: &str) -> Result<()> {
    db::set_setting(pool, keys::SUGGESTION_WORKOUT_NAME, name).await
}

/// The description belongs to whichever workout the name names, so it is
/// rewritten whenever the name is — otherwise a card ends up titled with one
/// workout and described with another.
pub async fn set_suggestion_workout_detail(pool: &SqlitePool, detail: &str) -> Result<()> {
    db::set_setting(pool, keys::SUGGESTION_WORKOUT_DETAIL, detail).await
}

// ── Fitness insight ──────────────────────────────────────────────────────────

pub async fn fitness_insight(pool: &SqlitePool) -> Result<Option<String>> {
    Ok(present(db::get_setting(pool, keys::FITNESS_INSIGHT).await?))
}

pub async fn set_fitness_insight(pool: &SqlitePool, text: &str) -> Result<()> {
    db::set_setting(pool, keys::FITNESS_INSIGHT, text).await
}

// ── Morning briefing ─────────────────────────────────────────────────────────

/// A cached briefing and the day it was written for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Briefing {
    pub text: String,
    /// ISO `YYYY-MM-DD` the briefing was generated for. Empty on briefings
    /// cached before the date was recorded — treat that as stale.
    pub written_for: String,
}

impl Briefing {
    /// Whether this briefing was written for `today`. A briefing from yesterday
    /// is worse than none: it opens with plans the rider has already ridden.
    pub fn is_for(&self, today: &str) -> bool {
        !self.written_for.is_empty() && self.written_for == today
    }
}

/// `None` until a briefing has been generated.
pub async fn briefing(pool: &SqlitePool) -> Result<Option<Briefing>> {
    let Some(text) = present(db::get_setting(pool, keys::BRIEFING_TEXT).await?) else {
        return Ok(None);
    };
    Ok(Some(Briefing {
        text,
        written_for: db::get_setting(pool, keys::BRIEFING_DATE)
            .await?
            .unwrap_or_default(),
    }))
}

/// Stamped with the date it is for, so the next read can tell whether it still
/// applies.
pub async fn save_briefing(pool: &SqlitePool, text: &str, written_for: &str) -> Result<()> {
    db::set_setting(pool, keys::BRIEFING_TEXT, text).await?;
    db::set_setting(pool, keys::BRIEFING_DATE, written_for).await
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
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite should connect");
        crate::data::migrate::run(&pool)
            .await
            .expect("migration should succeed");
        pool
    }

    // ── Suggestion ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn should_read_an_empty_suggestion_before_anything_is_generated() {
        let pool = test_pool().await;
        assert_eq!(
            load_suggestion(&pool).await.unwrap(),
            CachedSuggestion::default()
        );
        assert_eq!(suggestion_workout_name(&pool).await.unwrap(), None);
    }

    #[tokio::test]
    async fn should_round_trip_a_whole_suggestion() {
        let pool = test_pool().await;
        let suggestion = CachedSuggestion {
            response: "Ride easy today.".into(),
            workout_name: "Endurance 90".into(),
            workout_detail: "90 minutes at 65% FTP.".into(),
        };
        save_suggestion(&pool, &suggestion).await.unwrap();
        assert_eq!(load_suggestion(&pool).await.unwrap(), suggestion);
    }

    #[tokio::test]
    async fn should_let_the_briefing_set_the_workout_name_on_its_own() {
        // The morning briefing names a workout without generating a full
        // suggestion; the other two parts must be left alone.
        let pool = test_pool().await;
        save_suggestion(
            &pool,
            &CachedSuggestion {
                response: "keep me".into(),
                workout_name: "old".into(),
                workout_detail: "keep me too".into(),
            },
        )
        .await
        .unwrap();

        set_suggestion_workout_name(&pool, "Sweet Spot 2x20")
            .await
            .unwrap();

        let loaded = load_suggestion(&pool).await.unwrap();
        assert_eq!(loaded.workout_name, "Sweet Spot 2x20");
        assert_eq!(loaded.response, "keep me");
        assert_eq!(loaded.workout_detail, "keep me too");
    }

    #[tokio::test]
    async fn should_treat_a_blank_cached_name_as_nothing_cached() {
        let pool = test_pool().await;
        set_suggestion_workout_name(&pool, "").await.unwrap();
        assert_eq!(suggestion_workout_name(&pool).await.unwrap(), None);
    }

    // ── Fitness insight ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn should_round_trip_the_fitness_insight() {
        let pool = test_pool().await;
        assert_eq!(fitness_insight(&pool).await.unwrap(), None);

        set_fitness_insight(&pool, "Form is trending up.")
            .await
            .unwrap();
        assert_eq!(
            fitness_insight(&pool).await.unwrap().as_deref(),
            Some("Form is trending up.")
        );
    }

    // ── Briefing ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn should_have_no_briefing_before_one_is_written() {
        let pool = test_pool().await;
        assert_eq!(briefing(&pool).await.unwrap(), None);
    }

    #[tokio::test]
    async fn should_round_trip_a_briefing_with_its_date() {
        let pool = test_pool().await;
        save_briefing(&pool, "Sweet spot today.", "2026-08-08")
            .await
            .unwrap();

        let cached = briefing(&pool).await.unwrap().expect("briefing was saved");
        assert_eq!(cached.text, "Sweet spot today.");
        assert_eq!(cached.written_for, "2026-08-08");
    }

    #[tokio::test]
    async fn should_recognise_yesterdays_briefing_as_stale() {
        let pool = test_pool().await;
        save_briefing(&pool, "Yesterday's plan.", "2026-08-07")
            .await
            .unwrap();

        let cached = briefing(&pool).await.unwrap().unwrap();
        assert!(!cached.is_for("2026-08-08"));
        assert!(cached.is_for("2026-08-07"));
    }

    #[tokio::test]
    async fn should_treat_an_undated_briefing_as_stale() {
        // Cached before the date was recorded — it applies to no known day.
        let pool = test_pool().await;
        db::set_setting(&pool, keys::BRIEFING_TEXT, "Undated.")
            .await
            .unwrap();

        let cached = briefing(&pool).await.unwrap().unwrap();
        assert!(cached.written_for.is_empty());
        assert!(!cached.is_for("2026-08-08"));
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
            keys::SUGGESTION_RESPONSE,
            keys::SUGGESTION_WORKOUT_NAME,
            keys::SUGGESTION_WORKOUT_DETAIL,
            keys::FITNESS_INSIGHT,
            keys::BRIEFING_TEXT,
            keys::BRIEFING_DATE,
            keys::WEEKLY_RETROSPECTIVE,
            keys::MONTHLY_RETROSPECTIVE,
        ] {
            assert!(key.starts_with("ai."), "cache key not namespaced: {key}");
        }
    }
}
