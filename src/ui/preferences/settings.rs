//! The stored settings the preferences window is built around.

use sqlx::SqlitePool;

use crate::data::db;

pub struct PreferenceSettings {
    pub erg_ramp_rate: f64,
    pub sim_difficulty: f64,
    pub sim_max_gradient: f64,
    pub icu_athlete_id: String,
    pub icu_upload: bool,
    pub icu_sync: bool,
}

impl Default for PreferenceSettings {
    /// What each setting means when it has never been set.
    fn default() -> Self {
        Self {
            erg_ramp_rate: 25.0,
            sim_difficulty: 100.0,
            sim_max_gradient: 20.0,
            icu_athlete_id: String::new(),
            icu_upload: false,
            icu_sync: false,
        }
    }
}

/// Read every setting the window shows, in one pass off the GTK main thread.
///
/// An unset key falls back to its default; a failed *read* does not, because
/// the two are not the same thing — see [`super::show`].
pub async fn load_settings(pool: &SqlitePool) -> anyhow::Result<PreferenceSettings> {
    let defaults = PreferenceSettings::default();
    Ok(PreferenceSettings {
        erg_ramp_rate: db::get_setting(pool, "training.erg_ramp_rate")
            .await?
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(defaults.erg_ramp_rate),
        sim_difficulty: db::get_setting(pool, "training.sim_difficulty")
            .await?
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(defaults.sim_difficulty),
        sim_max_gradient: db::get_setting(pool, "training.sim_max_gradient")
            .await?
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(defaults.sim_max_gradient),
        icu_athlete_id: db::get_setting(pool, "intervals.athlete_id")
            .await?
            .unwrap_or_default(),
        icu_upload: db::get_setting(pool, "intervals.upload")
            .await?
            .map(|v| v == "1")
            .unwrap_or(defaults.icu_upload),
        icu_sync: db::get_setting(pool, "intervals.sync")
            .await?
            .map(|v| v == "1")
            .unwrap_or(defaults.icu_sync),
    })
}
