//! Typed access to the rider's stored settings.
//!
//! Every settings key is named exactly once, in [`keys`], and every default
//! lives beside the type it belongs to. Before this module the keys were bare
//! string literals spread across the UI, each read site re-deriving its own
//! parse-and-fallback chain — four keys were being read in two places with
//! independently written defaults, agreeing only by coincidence.
//!
//! Reads follow the rule the preferences window already documents: **an unset
//! key falls back to its default; a failed read does not.** The two mean
//! different things — one is a rider who has never touched the setting, the
//! other is a database that could not be read — and collapsing them shows the
//! rider a value that is not the one saved.
//!
//! Cached AI output also lives in the settings table but is not a setting; it
//! has its own module, [`super::ai_cache`].

use anyhow::Result;
use sqlx::SqlitePool;

use super::db;

/// The settings keys this app reads and writes.
///
/// `reset_athlete_data` also names several of these when it decides what
/// survives a data wipe, and two keys it preserves — `anthropic.api_key` and
/// `intervals.api_key` — are legacy rows that moved to the keyring and are kept
/// only so an old database is not silently stripped.
pub mod keys {
    pub const ERG_RAMP_RATE: &str = "training.erg_ramp_rate";
    pub const SIM_DIFFICULTY: &str = "training.sim_difficulty";
    pub const SIM_MAX_GRADIENT: &str = "training.sim_max_gradient";
    pub const INTERVAL_CUES: &str = "training.interval_cues";

    pub const INTERVALS_ATHLETE_ID: &str = "intervals.athlete_id";
    pub const INTERVALS_UPLOAD: &str = "intervals.upload";
    pub const INTERVALS_SYNC: &str = "intervals.sync";

    pub const WINDOW_WIDTH: &str = "window.width";
    pub const WINDOW_HEIGHT: &str = "window.height";

    pub const FIRST_USE_COMPLETE: &str = "first_use_complete";
    pub const COACHING_CONTEXT: &str = "coaching.athlete_context";
}

/// Booleans are stored as `"1"` / `"0"`; anything else reads as unset.
fn flag(raw: Option<String>, default: bool) -> bool {
    raw.map(|v| v == "1").unwrap_or(default)
}

fn flag_value(on: bool) -> &'static str {
    if on {
        "1"
    } else {
        "0"
    }
}

// ── Training ─────────────────────────────────────────────────────────────────

/// How the trainer is driven. Read at startup and again by the preferences
/// window, which is why the defaults have to live in one place.
#[derive(Debug, Clone, PartialEq)]
pub struct TrainingSettings {
    /// Maximum watts per second the ERG target may move. 0 means instant.
    pub erg_ramp_rate: u32,
    /// SIM resistance scale as a **percentage** — 100 rides the full gradient.
    /// Stored in percent and divided down at the point of use.
    pub sim_difficulty_pct: f32,
    /// Gradient ceiling in percent, whatever the route actually climbs.
    pub sim_max_gradient_pct: f32,
    /// Whether the player narrates each interval. On by default: a rider who
    /// has never opened Preferences is the one who benefits most from being
    /// told which rep they are on.
    pub interval_cues: bool,
}

impl Default for TrainingSettings {
    fn default() -> Self {
        Self {
            erg_ramp_rate: 25,
            sim_difficulty_pct: 100.0,
            sim_max_gradient_pct: 20.0,
            interval_cues: true,
        }
    }
}

/// Read the training settings in one pass. Fails only if the database does.
pub async fn load_training(pool: &SqlitePool) -> Result<TrainingSettings> {
    let d = TrainingSettings::default();
    Ok(TrainingSettings {
        erg_ramp_rate: db::get_setting(pool, keys::ERG_RAMP_RATE)
            .await?
            .and_then(|v| v.parse().ok())
            .unwrap_or(d.erg_ramp_rate),
        sim_difficulty_pct: db::get_setting(pool, keys::SIM_DIFFICULTY)
            .await?
            .and_then(|v| v.parse().ok())
            .unwrap_or(d.sim_difficulty_pct),
        sim_max_gradient_pct: db::get_setting(pool, keys::SIM_MAX_GRADIENT)
            .await?
            .and_then(|v| v.parse().ok())
            .unwrap_or(d.sim_max_gradient_pct),
        interval_cues: flag(
            db::get_setting(pool, keys::INTERVAL_CUES).await?,
            d.interval_cues,
        ),
    })
}

pub async fn set_erg_ramp_rate(pool: &SqlitePool, watts_per_sec: u32) -> Result<()> {
    db::set_setting(pool, keys::ERG_RAMP_RATE, &watts_per_sec.to_string()).await
}

pub async fn set_sim_difficulty_pct(pool: &SqlitePool, percent: f32) -> Result<()> {
    db::set_setting(pool, keys::SIM_DIFFICULTY, &percent.to_string()).await
}

pub async fn set_sim_max_gradient_pct(pool: &SqlitePool, percent: f32) -> Result<()> {
    db::set_setting(pool, keys::SIM_MAX_GRADIENT, &percent.to_string()).await
}

pub async fn set_interval_cues(pool: &SqlitePool, on: bool) -> Result<()> {
    db::set_setting(pool, keys::INTERVAL_CUES, flag_value(on)).await
}

// ── Intervals.icu ────────────────────────────────────────────────────────────

/// The Intervals.icu link. The API key itself is not here — it lives in the
/// keyring (see [`super::keystore`]), never in the database.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IntervalsSettings {
    /// Empty when the rider has not connected an account.
    pub athlete_id: String,
    /// Upload finished rides.
    pub upload: bool,
    /// Pull activities and planned workouts back down.
    pub sync: bool,
}

pub async fn load_intervals(pool: &SqlitePool) -> Result<IntervalsSettings> {
    let d = IntervalsSettings::default();
    Ok(IntervalsSettings {
        athlete_id: db::get_setting(pool, keys::INTERVALS_ATHLETE_ID)
            .await?
            .unwrap_or(d.athlete_id),
        upload: flag(
            db::get_setting(pool, keys::INTERVALS_UPLOAD).await?,
            d.upload,
        ),
        sync: flag(db::get_setting(pool, keys::INTERVALS_SYNC).await?, d.sync),
    })
}

pub async fn set_intervals_athlete_id(pool: &SqlitePool, id: &str) -> Result<()> {
    db::set_setting(pool, keys::INTERVALS_ATHLETE_ID, id).await
}

pub async fn set_intervals_upload(pool: &SqlitePool, on: bool) -> Result<()> {
    db::set_setting(pool, keys::INTERVALS_UPLOAD, flag_value(on)).await
}

pub async fn set_intervals_sync(pool: &SqlitePool, on: bool) -> Result<()> {
    db::set_setting(pool, keys::INTERVALS_SYNC, flag_value(on)).await
}

// ── Window geometry ──────────────────────────────────────────────────────────

/// The size the window was last closed at. `None` means never recorded, and the
/// window should keep its own default rather than invent one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WindowSettings {
    pub width: Option<i32>,
    pub height: Option<i32>,
}

impl WindowSettings {
    /// Both dimensions, or `None` if either is missing — half a remembered size
    /// is not usable.
    pub fn size(self) -> Option<(i32, i32)> {
        self.width.zip(self.height)
    }
}

pub async fn load_window(pool: &SqlitePool) -> Result<WindowSettings> {
    Ok(WindowSettings {
        width: db::get_setting(pool, keys::WINDOW_WIDTH)
            .await?
            .and_then(|v| v.parse().ok()),
        height: db::get_setting(pool, keys::WINDOW_HEIGHT)
            .await?
            .and_then(|v| v.parse().ok()),
    })
}

/// Written together on close — a width without its height is not useful.
pub async fn set_window_size(pool: &SqlitePool, width: i32, height: i32) -> Result<()> {
    db::set_setting(pool, keys::WINDOW_WIDTH, &width.to_string()).await?;
    db::set_setting(pool, keys::WINDOW_HEIGHT, &height.to_string()).await
}

// ── First run ────────────────────────────────────────────────────────────────

/// Whether the rider has finished onboarding.
pub async fn first_use_complete(pool: &SqlitePool) -> Result<bool> {
    Ok(flag(
        db::get_setting(pool, keys::FIRST_USE_COMPLETE).await?,
        false,
    ))
}

pub async fn mark_first_use_complete(pool: &SqlitePool) -> Result<()> {
    db::set_setting(pool, keys::FIRST_USE_COMPLETE, flag_value(true)).await
}

// ── Coaching context ─────────────────────────────────────────────────────────

/// Free text the rider writes about themselves, sent with every AI prompt.
/// Empty when never filled in.
pub async fn coaching_context(pool: &SqlitePool) -> Result<String> {
    Ok(db::get_setting(pool, keys::COACHING_CONTEXT)
        .await?
        .unwrap_or_default())
}

pub async fn set_coaching_context(pool: &SqlitePool, context: &str) -> Result<()> {
    db::set_setting(pool, keys::COACHING_CONTEXT, context).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, migrated in-memory database. Never the real XDG path
    /// (CLAUDE.md §3.5).
    async fn test_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite should connect");
        crate::data::migrate::run(&pool)
            .await
            .expect("migration should succeed");
        pool
    }

    // ── Training ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn should_use_training_defaults_when_nothing_was_ever_saved() {
        let pool = test_pool().await;
        assert_eq!(
            load_training(&pool).await.unwrap(),
            TrainingSettings::default()
        );
    }

    #[tokio::test]
    async fn should_round_trip_every_training_setting() {
        let pool = test_pool().await;
        set_erg_ramp_rate(&pool, 40).await.unwrap();
        set_sim_difficulty_pct(&pool, 75.0).await.unwrap();
        set_sim_max_gradient_pct(&pool, 12.5).await.unwrap();
        set_interval_cues(&pool, false).await.unwrap();

        let loaded = load_training(&pool).await.unwrap();
        assert_eq!(loaded.erg_ramp_rate, 40);
        assert_eq!(loaded.sim_difficulty_pct, 75.0);
        assert_eq!(loaded.sim_max_gradient_pct, 12.5);
        assert!(!loaded.interval_cues);
    }

    #[tokio::test]
    async fn should_default_interval_cues_on() {
        let pool = test_pool().await;
        assert!(load_training(&pool).await.unwrap().interval_cues);
    }

    #[tokio::test]
    async fn should_keep_interval_cues_off_once_turned_off() {
        // The flag helper treats anything but "1" as unset, so an explicit "off"
        // has to survive a reload rather than falling back to the default.
        let pool = test_pool().await;
        set_interval_cues(&pool, false).await.unwrap();
        assert!(!load_training(&pool).await.unwrap().interval_cues);
        set_interval_cues(&pool, true).await.unwrap();
        assert!(load_training(&pool).await.unwrap().interval_cues);
    }

    #[tokio::test]
    async fn should_fall_back_to_the_default_when_a_stored_value_is_unparseable() {
        // A hand-edited or corrupted row must not take the trainer with it.
        let pool = test_pool().await;
        db::set_setting(&pool, keys::ERG_RAMP_RATE, "not a number")
            .await
            .unwrap();
        assert_eq!(
            load_training(&pool).await.unwrap().erg_ramp_rate,
            TrainingSettings::default().erg_ramp_rate
        );
    }

    #[tokio::test]
    async fn should_keep_an_erg_ramp_rate_of_zero_rather_than_defaulting_it() {
        // 0 means "no smoothing" and is a real choice, not an absent one.
        let pool = test_pool().await;
        set_erg_ramp_rate(&pool, 0).await.unwrap();
        assert_eq!(load_training(&pool).await.unwrap().erg_ramp_rate, 0);
    }

    // ── Intervals.icu ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn should_default_intervals_to_disconnected_and_off() {
        let pool = test_pool().await;
        let loaded = load_intervals(&pool).await.unwrap();
        assert_eq!(loaded, IntervalsSettings::default());
        assert!(loaded.athlete_id.is_empty());
        assert!(!loaded.upload);
        assert!(!loaded.sync);
    }

    #[tokio::test]
    async fn should_round_trip_the_intervals_link() {
        let pool = test_pool().await;
        set_intervals_athlete_id(&pool, "i12345").await.unwrap();
        set_intervals_upload(&pool, true).await.unwrap();
        set_intervals_sync(&pool, true).await.unwrap();

        let loaded = load_intervals(&pool).await.unwrap();
        assert_eq!(loaded.athlete_id, "i12345");
        assert!(loaded.upload);
        assert!(loaded.sync);
    }

    #[tokio::test]
    async fn should_turn_a_flag_back_off() {
        // Writing "0" has to read back as false, not as unset-and-defaulted.
        let pool = test_pool().await;
        set_intervals_upload(&pool, true).await.unwrap();
        set_intervals_upload(&pool, false).await.unwrap();
        assert!(!load_intervals(&pool).await.unwrap().upload);
    }

    #[tokio::test]
    async fn should_read_an_unexpected_flag_value_as_off() {
        let pool = test_pool().await;
        db::set_setting(&pool, keys::INTERVALS_UPLOAD, "yes")
            .await
            .unwrap();
        assert!(!load_intervals(&pool).await.unwrap().upload);
    }

    // ── Window ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn should_report_no_window_size_before_one_is_recorded() {
        let pool = test_pool().await;
        assert_eq!(load_window(&pool).await.unwrap().size(), None);
    }

    #[tokio::test]
    async fn should_round_trip_the_window_size() {
        let pool = test_pool().await;
        set_window_size(&pool, 1280, 800).await.unwrap();
        assert_eq!(load_window(&pool).await.unwrap().size(), Some((1280, 800)));
    }

    #[tokio::test]
    async fn should_ignore_half_a_remembered_window_size() {
        let pool = test_pool().await;
        db::set_setting(&pool, keys::WINDOW_WIDTH, "1280")
            .await
            .unwrap();
        let loaded = load_window(&pool).await.unwrap();
        assert_eq!(loaded.width, Some(1280));
        assert_eq!(loaded.size(), None);
    }

    // ── First run and coaching context ───────────────────────────────────────

    #[tokio::test]
    async fn should_treat_a_new_database_as_a_first_run() {
        let pool = test_pool().await;
        assert!(!first_use_complete(&pool).await.unwrap());
        mark_first_use_complete(&pool).await.unwrap();
        assert!(first_use_complete(&pool).await.unwrap());
    }

    #[tokio::test]
    async fn should_round_trip_the_coaching_context() {
        let pool = test_pool().await;
        assert_eq!(coaching_context(&pool).await.unwrap(), "");

        set_coaching_context(&pool, "Masters rider, 3 rides a week.")
            .await
            .unwrap();
        assert_eq!(
            coaching_context(&pool).await.unwrap(),
            "Masters rider, 3 rides a week."
        );
    }

    // ── Keys ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn should_not_let_two_settings_share_a_key() {
        let all = [
            keys::ERG_RAMP_RATE,
            keys::SIM_DIFFICULTY,
            keys::SIM_MAX_GRADIENT,
            keys::INTERVAL_CUES,
            keys::INTERVALS_ATHLETE_ID,
            keys::INTERVALS_UPLOAD,
            keys::INTERVALS_SYNC,
            keys::WINDOW_WIDTH,
            keys::WINDOW_HEIGHT,
            keys::FIRST_USE_COMPLETE,
            keys::COACHING_CONTEXT,
        ];
        let mut seen = std::collections::HashSet::new();
        for key in all {
            assert!(seen.insert(key), "duplicate settings key: {key}");
        }
    }

    #[tokio::test]
    async fn should_keep_settings_groups_independent() {
        // Writing one group must not disturb another — they share a table.
        let pool = test_pool().await;
        set_erg_ramp_rate(&pool, 40).await.unwrap();
        set_intervals_upload(&pool, true).await.unwrap();
        set_window_size(&pool, 1280, 800).await.unwrap();

        assert_eq!(load_training(&pool).await.unwrap().erg_ramp_rate, 40);
        assert!(load_intervals(&pool).await.unwrap().upload);
        assert_eq!(load_window(&pool).await.unwrap().size(), Some((1280, 800)));
    }
}
