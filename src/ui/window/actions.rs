//! Application actions, keyboard shortcuts, and what happens on close.

use adw::prelude::*;
use gtk::{gio, glib};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use sqlx::SqlitePool;

use crate::data::{athlete::AthleteProfile, db};
use crate::training::engine::WorkoutEngine;

/// Register the app-wide actions and their accelerators.
#[allow(clippy::too_many_arguments)]
pub fn install(
    app: &adw::Application,
    window: &adw::ApplicationWindow,
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
    athlete_rc: Rc<RefCell<AthleteProfile>>,
    engine_rc: Rc<RefCell<WorkoutEngine>>,
    sim_difficulty: Rc<Cell<f32>>,
    sim_max_grade: Rc<Cell<f32>>,
) {
    app.set_accels_for_action("app.quit", &["<Control>q"]);
    let close_action = gio::SimpleAction::new("close", None);
    let window_for_close_action = window.clone();
    close_action.connect_activate(move |_, _| window_for_close_action.close());
    window.add_action(&close_action);
    app.set_accels_for_action("win.close", &["<Control>w"]);

    // ── App actions ──────────────────────────────────────────────────────
    let window_for_about = window.clone();
    let about_action = gio::SimpleAction::new("about", None);
    about_action.connect_activate(move |_, _| {
        let dialog = adw::AboutDialog::builder()
            .application_name("Cycle")
            .application_icon("io.github.rorynuijens.Cycle")
            .version(env!("CARGO_PKG_VERSION"))
            .developer_name("Rory Nuijens")
            .build();
        dialog.present(Some(&window_for_about));
    });
    app.add_action(&about_action);

    let engine_for_prefs = Rc::clone(&engine_rc);
    let pool_for_prefs = pool.clone();
    let rt_for_prefs = rt_handle.clone();
    let window_for_prefs = window.clone();
    let athlete_for_prefs = Rc::clone(&athlete_rc);
    let sim_difficulty_for_prefs = Rc::clone(&sim_difficulty);
    let sim_max_grade_for_prefs = Rc::clone(&sim_max_grade);
    let prefs_action = gio::SimpleAction::new("preferences", None);
    prefs_action.connect_activate(move |_, _| {
        let current_athlete = athlete_for_prefs.borrow().clone();
        let athlete_on_save = Rc::clone(&athlete_for_prefs);
        let engine_for_erg = Rc::clone(&engine_for_prefs);
        let sim_difficulty_prefs = Rc::clone(&sim_difficulty_for_prefs);
        let sim_max_grade_prefs = Rc::clone(&sim_max_grade_for_prefs);
        crate::ui::preferences::show(
            &window_for_prefs,
            current_athlete,
            pool_for_prefs.clone(),
            rt_for_prefs.clone(),
            // One write, one reader set: every page and the running engine
            // share this cell, so there is nothing else to keep in step.
            move |new_athlete| {
                *athlete_on_save.borrow_mut() = new_athlete;
            },
            move |rate| {
                engine_for_erg.borrow_mut().erg_ramp_rate = rate;
            },
            move |difficulty_pct, max_grade_pct| {
                sim_difficulty_prefs.set(difficulty_pct as f32 / 100.0);
                sim_max_grade_prefs.set(max_grade_pct as f32);
            },
        );
    });
    app.add_action(&prefs_action);
}

/// Save the window geometry, and confirm before discarding a ride in progress.
pub fn connect_close(
    window: &adw::ApplicationWindow,
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
    workout_active: Rc<Cell<bool>>,
    route_timer_alive: Rc<Cell<bool>>,
) {
    // A ride exists only in memory until it ends: nothing reaches the database
    // until `finish_session` runs. Closing the window mid-ride would therefore
    // discard it with no warning, so confirm first. "End Workout" / "End Ride"
    // stay the paths that save — this dialog only makes the loss deliberate.
    //
    // Structured workouts and route rides track their in-progress state
    // separately, so both flags are consulted.
    let pool_close = pool.clone();
    let rt_close = rt_handle.clone();
    let workout_active_close = Rc::clone(&workout_active);
    let route_active_close = Rc::clone(&route_timer_alive);
    window.connect_close_request(move |win| {
        let width = win.width();
        let height = win.height();
        let p = pool_close.clone();
        rt_close.spawn(async move {
            let _ = db::set_setting(&p, "window.width", &width.to_string()).await;
            let _ = db::set_setting(&p, "window.height", &height.to_string()).await;
        });

        if !workout_active_close.get() && !route_active_close.get() {
            return glib::Propagation::Proceed;
        }

        let dialog = adw::AlertDialog::builder()
            .heading("Quit with a ride in progress?")
            .body(
                "The ride so far has been saved. Cycle will offer to recover it \
             the next time it starts.\n\n\
             To finish it properly instead, cancel and end the ride from the \
             ride screen.",
            )
            .build();
        dialog.add_response("cancel", "_Keep Riding");
        dialog.add_response("quit", "_Quit");
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");

        let win_resp = win.clone();
        dialog.connect_response(None, move |_, response| {
            if response == "quit" {
                // destroy() rather than close(): close() re-emits close-request
                // and would land back in this handler.
                win_resp.destroy();
            }
        });
        dialog.present(Some(win));
        glib::Propagation::Stop
    });
}
