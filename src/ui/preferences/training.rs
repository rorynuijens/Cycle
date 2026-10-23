//! The Training page: how the trainer responds during a ride.
//!
//! Both settings take effect immediately on the running engine as well as being
//! stored, so a change can be felt without restarting the ride.

use adw::prelude::*;
use sqlx::SqlitePool;
use std::rc::Rc;

use crate::data::biome::{Scenery, WorldLook};
use crate::data::settings::{self, TrainingSettings};
use crate::ui::spawn_write;

/// Build the Training page.
#[allow(clippy::too_many_arguments)] // preferences window wiring
pub fn build(
    settings: &TrainingSettings,
    workout_look: WorldLook,
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
    on_erg_rate_changed: impl Fn(u32) + 'static,
    on_sim_changed: impl Fn(u32, u32) + 'static,
    on_cues_changed: impl Fn(bool) + 'static,
    on_auto_pause_changed: impl Fn(bool) + 'static,
) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("Training")
        .icon_name("media-playback-start-symbolic")
        .build();

    let cues_group = adw::PreferencesGroup::builder()
        .title("During a Ride")
        .build();
    let cues_row = adw::SwitchRow::builder()
        .title("Interval Cues")
        .subtitle("Which rep you are on, how many are left, and how to ride it")
        .active(settings.interval_cues)
        .build();
    cues_group.add(&cues_row);
    let auto_pause_row = adw::SwitchRow::builder()
        .title("Auto-Pause")
        .subtitle(
            "Pause after 10 seconds without pedalling, and carry on after \
             10 seconds of pedalling again",
        )
        .active(settings.auto_pause)
        .build();
    cues_group.add(&auto_pause_row);
    page.add(&cues_group);

    let erg_group = adw::PreferencesGroup::builder()
        .title("ERG Mode")
        .description(
            "Controls how quickly the trainer adjusts to a new power target. \
             Lower values feel smoother; set to 0 for an instant step change.",
        )
        .build();
    let ramp_adj = gtk::Adjustment::new(settings.erg_ramp_rate as f64, 0.0, 100.0, 1.0, 5.0, 0.0);
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
    let difficulty_adj = gtk::Adjustment::new(
        settings.sim_difficulty_pct as f64,
        0.0,
        100.0,
        5.0,
        10.0,
        0.0,
    );
    let difficulty_row = adw::SpinRow::new(Some(&difficulty_adj), 5.0, 0);
    difficulty_row.set_title("Trainer Difficulty");
    difficulty_row.set_subtitle("Percentage of the real gradient sent to the trainer");
    sim_group.add(&difficulty_row);

    let max_grade_adj = gtk::Adjustment::new(
        settings.sim_max_gradient_pct as f64,
        5.0,
        20.0,
        1.0,
        5.0,
        0.0,
    );
    let max_grade_row = adw::SpinRow::new(Some(&max_grade_adj), 1.0, 0);
    max_grade_row.set_title("Maximum Gradient");
    max_grade_row.set_subtitle("Climbs steeper than this are capped (%)");
    sim_group.add(&max_grade_row);
    page.add(&sim_group);

    // Read when each workout opens, so a change here waits for the next one:
    // the world is built once, as the ride starts.
    let world_group = adw::PreferencesGroup::builder()
        .title("3D World")
        .description(
            "The road a workout is ridden along in 3D climbs through the hard \
             intervals. It is only scenery: the trainer still holds the target.",
        )
        .build();
    let labels: Vec<&str> = Scenery::CHOICES.iter().map(|s| s.label()).collect();
    let scenery_row = adw::ComboRow::builder()
        .title("Workout Scenery")
        .subtitle("Routes keep the scenery picked for each of them")
        .model(&gtk::StringList::new(&labels))
        .selected(
            Scenery::CHOICES
                .iter()
                .position(|s| *s == workout_look.scenery)
                .unwrap_or(0) as u32,
        )
        .build();
    world_group.add(&scenery_row);
    let animals_row = adw::SwitchRow::builder()
        .title("Animals")
        .subtitle("Herds, birds and animal encounters, on every route and workout")
        .active(workout_look.animals)
        .build();
    world_group.add(&animals_row);
    page.add(&world_group);

    {
        let pool = pool.clone();
        let rt_handle = rt_handle.clone();
        scenery_row.connect_selected_notify(move |row| {
            let Some(&picked) = Scenery::CHOICES.get(row.selected() as usize) else {
                return;
            };
            spawn_write(
                &rt_handle,
                &pool,
                "the workout scenery",
                move |pool| async move { settings::set_workout_scenery(&pool, picked).await },
            );
        });
    }
    {
        let pool = pool.clone();
        let rt_handle = rt_handle.clone();
        animals_row.connect_active_notify(move |row| {
            let on = row.is_active();
            spawn_write(
                &rt_handle,
                &pool,
                "the animals setting",
                move |pool| async move { settings::set_animals_3d(&pool, on).await },
            );
        });
    }

    {
        let pool = pool.clone();
        let rt_handle = rt_handle.clone();
        cues_row.connect_active_notify(move |row| {
            let on = row.is_active();
            on_cues_changed(on);
            spawn_write(
                &rt_handle,
                &pool,
                "the interval cues setting",
                move |pool| async move { settings::set_interval_cues(&pool, on).await },
            );
        });
    }

    {
        let pool = pool.clone();
        let rt_handle = rt_handle.clone();
        auto_pause_row.connect_active_notify(move |row| {
            let on = row.is_active();
            on_auto_pause_changed(on);
            spawn_write(
                &rt_handle,
                &pool,
                "the auto-pause setting",
                move |pool| async move { settings::set_auto_pause(&pool, on).await },
            );
        });
    }

    {
        let pool = pool.clone();
        let rt_handle = rt_handle.clone();
        ramp_row.connect_value_notify(move |row| {
            let rate = row.value() as u32;
            on_erg_rate_changed(rate);
            spawn_write(
                &rt_handle,
                &pool,
                "the ERG ramp rate",
                move |pool| async move { settings::set_erg_ramp_rate(&pool, rate).await },
            );
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
            spawn_write(
                &rt_handle,
                &pool,
                "the SIM difficulty",
                move |pool| async move {
                    settings::set_sim_difficulty_pct(&pool, difficulty as f32).await
                },
            );
            spawn_write(
                &rt_handle,
                &pool,
                "the maximum gradient",
                move |pool| async move {
                    settings::set_sim_max_gradient_pct(&pool, max_gradient as f32).await
                },
            );
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
