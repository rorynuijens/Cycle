//! The Training page: how the trainer responds during a ride.
//!
//! Both settings take effect immediately on the running engine as well as being
//! stored, so a change can be felt without restarting the ride.

use adw::prelude::*;
use sqlx::SqlitePool;
use std::rc::Rc;

use crate::data::db;

use super::settings::PreferenceSettings;

/// Store a numeric training setting, logging rather than surfacing a failure —
/// the value is already applied to the running engine either way.
fn save_setting(
    pool: &SqlitePool,
    rt_handle: &tokio::runtime::Handle,
    key: &'static str,
    value: u32,
) {
    let pool = pool.clone();
    rt_handle.spawn(async move {
        if let Err(e) = db::set_setting(&pool, key, &value.to_string()).await {
            tracing::error!("save {key} failed: {e}");
        }
    });
}

/// Build the Training page.
pub fn build(
    settings: &PreferenceSettings,
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
    on_erg_rate_changed: impl Fn(u32) + 'static,
    on_sim_changed: impl Fn(u32, u32) + 'static,
) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("Training")
        .icon_name("media-playback-start-symbolic")
        .build();

    let erg_group = adw::PreferencesGroup::builder()
        .title("ERG Mode")
        .description(
            "Controls how quickly the trainer adjusts to a new power target. \
             Lower values feel smoother; set to 0 for an instant step change.",
        )
        .build();
    let ramp_adj = gtk::Adjustment::new(settings.erg_ramp_rate, 0.0, 100.0, 1.0, 5.0, 0.0);
    let ramp_row = adw::SpinRow::new(Some(&ramp_adj), 1.0, 0);
    ramp_row.set_title("Ramp Rate");
    ramp_row.set_subtitle("Watts per second (0 = instant)");
    erg_group.add(&ramp_row);
    page.add(&erg_group);

    let sim_group = adw::PreferencesGroup::builder()
        .title("SIM Mode")
        .description(
            "How road gradients from a GPX route reach the trainer. \
             Lower the difficulty if steep climbs force you out of gears.",
        )
        .build();
    let difficulty_adj = gtk::Adjustment::new(settings.sim_difficulty, 0.0, 100.0, 5.0, 10.0, 0.0);
    let difficulty_row = adw::SpinRow::new(Some(&difficulty_adj), 5.0, 0);
    difficulty_row.set_title("Trainer Difficulty");
    difficulty_row.set_subtitle("Percentage of the real gradient sent to the trainer");
    sim_group.add(&difficulty_row);

    let max_grade_adj = gtk::Adjustment::new(settings.sim_max_gradient, 5.0, 20.0, 1.0, 5.0, 0.0);
    let max_grade_row = adw::SpinRow::new(Some(&max_grade_adj), 1.0, 0);
    max_grade_row.set_title("Maximum Gradient");
    max_grade_row.set_subtitle("Climbs steeper than this are capped (%)");
    sim_group.add(&max_grade_row);
    page.add(&sim_group);

    {
        let pool = pool.clone();
        let rt_handle = rt_handle.clone();
        ramp_row.connect_value_notify(move |row| {
            let rate = row.value() as u32;
            on_erg_rate_changed(rate);
            save_setting(&pool, &rt_handle, "training.erg_ramp_rate", rate);
        });
    }

    // Both SIM rows report the pair, so the ride loop takes a single update.
    let on_sim_changed: Rc<dyn Fn(u32, u32)> = Rc::new(on_sim_changed);
    let apply_sim: Rc<dyn Fn(u32, u32)> = {
        let on_sim_changed = Rc::clone(&on_sim_changed);
        let pool = pool.clone();
        let rt_handle = rt_handle.clone();
        Rc::new(move |difficulty, max_gradient| {
            on_sim_changed(difficulty, max_gradient);
            save_setting(&pool, &rt_handle, "training.sim_difficulty", difficulty);
            save_setting(&pool, &rt_handle, "training.sim_max_gradient", max_gradient);
        })
    };

    {
        let apply = Rc::clone(&apply_sim);
        let max_grade_row = max_grade_row.clone();
        difficulty_row.connect_value_notify(move |row| {
            apply(row.value() as u32, max_grade_row.value() as u32);
        });
    }
    {
        let apply = Rc::clone(&apply_sim);
        let difficulty_row = difficulty_row.clone();
        max_grade_row.connect_value_notify(move |row| {
            apply(difficulty_row.value() as u32, row.value() as u32);
        });
    }

    page
}
