//! The preferences window: four pages, each writing through as it changes.

mod athlete;
mod data;
mod integrations;
mod training;

use adw::prelude::*;
use sqlx::SqlitePool;
use std::rc::Rc;

use crate::data::athlete::AthleteProfile;
use crate::data::settings::{self, IntervalsSettings, TrainingSettings};

/// Create and present the modal preferences window.
///
/// Changes apply immediately — no Save button. `on_saved` is called whenever the
/// athlete profile changes; `on_erg_rate_changed` whenever the ERG ramp rate
/// changes; `on_sim_changed` with `(difficulty_percent, max_gradient_percent)`
/// whenever either SIM setting changes.
///
/// The stored settings are read before the window is built, off the GTK main
/// thread (CLAUDE.md §2.3). They used to be read with `block_on` partway through
/// construction, which stalls the GLib loop whenever SQLite is busy — and it is
/// busy every 30 s during a ride, when the session checkpoint writes.
///
/// If the read fails the window does not open. Opening it on defaults would show
/// the rider values that are not the ones saved, and a rider who "corrects" one
/// of them writes the wrong value over a good one.
pub fn show(
    parent: &adw::ApplicationWindow,
    athlete: AthleteProfile,
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
    on_saved: impl Fn(AthleteProfile) + 'static,
    on_erg_rate_changed: impl Fn(u32) + 'static,
    on_sim_changed: impl Fn(u32, u32) + 'static,
) {
    let parent = parent.clone();
    let pool_load = pool.clone();
    let rt_build = rt_handle.clone();
    crate::ui::spawn_to_main(
        &rt_handle,
        async move {
            Ok::<_, anyhow::Error>((
                settings::load_training(&pool_load).await?,
                settings::load_intervals(&pool_load).await?,
            ))
        },
        move |result| match result {
            Ok((training_settings, intervals_settings)) => build_and_present(
                &parent,
                athlete,
                pool,
                rt_build,
                training_settings,
                intervals_settings,
                on_saved,
                on_erg_rate_changed,
                on_sim_changed,
            ),
            Err(e) => {
                tracing::error!("Could not read your settings: {e}");
                let dialog = adw::AlertDialog::builder()
                    .heading("Could not open Preferences")
                    .body(
                        "Your settings could not be read. Preferences was not opened \
                         so that nothing overwrites them.",
                    )
                    .build();
                dialog.add_response("ok", "_OK");
                dialog.set_default_response(Some("ok"));
                dialog.present(Some(&parent));
            }
        },
    );
}

#[allow(clippy::too_many_arguments)] // preferences window wiring
fn build_and_present(
    parent: &adw::ApplicationWindow,
    athlete: AthleteProfile,
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
    training_settings: TrainingSettings,
    intervals_settings: IntervalsSettings,
    on_saved: impl Fn(AthleteProfile) + 'static,
    on_erg_rate_changed: impl Fn(u32) + 'static,
    on_sim_changed: impl Fn(u32, u32) + 'static,
) {
    let win = adw::PreferencesWindow::builder()
        .transient_for(parent)
        .modal(true)
        .search_enabled(false)
        .title("Preferences")
        .build();

    win.add(&athlete::build(
        &win,
        athlete,
        pool.clone(),
        rt_handle.clone(),
        Rc::new(on_saved),
    ));
    win.add(&training::build(
        &training_settings,
        pool.clone(),
        rt_handle.clone(),
        on_erg_rate_changed,
        on_sim_changed,
    ));
    win.add(&integrations::build(
        &win,
        &intervals_settings,
        pool.clone(),
        rt_handle.clone(),
    ));
    win.add(&data::build(&win, parent, pool, rt_handle));

    win.present();
}
