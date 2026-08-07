use adw::prelude::*;
use gtk::{gio, glib};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use async_channel::{Receiver, Sender};
use sqlx::SqlitePool;

use super::pages::{
    calendar::CalendarPage, coaching::CoachingPage, dashboard::DashboardPage, devices::DevicesPage,
    fitness::FitnessPage, library::LibraryPage, player::PlayerPage, route_player::RoutePlayerPage,
    summary::SummaryPage,
};
use crate::data::{
    athlete::AthleteProfile,
    db::{self, SavedDevice},
    route::Route,
    workout::Workout,
};
use crate::devices::manager::{DeviceCommand, DeviceEvent, DeviceType};
use crate::training::engine::{EngineState, WorkoutEngine};

/// Ends a ride: summary page, RPE prompt, save and upload. Takes the finished
/// session, the name to file it under, and the workout plan it was ridden
/// against (`None` for a route ride).
type FinishSession =
    Rc<dyn Fn(crate::data::session::Session, String, Option<Vec<crate::data::workout::Segment>>)>;

/// How often a ride in progress is written to disk. The trade is how much of a
/// ride a crash can cost against how often a multi-hundred-KB blob is rewritten;
/// 30 s keeps the worst case to half a minute of pedalling.
const CHECKPOINT_INTERVAL_SECS: u64 = 30;

pub struct CycleGtkWindow {
    pub window: adw::ApplicationWindow,
    toast_overlay: adw::ToastOverlay,
    #[allow(dead_code)] // kept alive so connections stay open for the window's lifetime
    pool: SqlitePool,
}

impl CycleGtkWindow {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        app: &adw::Application,
        cmd_tx: Sender<DeviceCommand>,
        event_rx: Receiver<DeviceEvent>,
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        saved_devices: Vec<SavedDevice>,
        athlete: AthleteProfile,
        workout: Workout,
        workouts: Vec<Workout>,
    ) -> Self {
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Cycle")
            .default_width(1100)
            .default_height(700)
            .icon_name("io.github.rorynuijens.Cycle")
            .build();
        // Allow compositor to resize for window snap (Super+Left/Right)
        window.set_size_request(640, 480);

        let toast_overlay = adw::ToastOverlay::new();

        let obj = Self {
            window,
            toast_overlay,
            pool: pool.clone(),
        };
        obj.build_ui(
            app,
            cmd_tx,
            event_rx,
            pool,
            rt_handle,
            saved_devices,
            athlete,
            workout,
            workouts,
        );
        obj
    }

    pub fn present(&self) {
        self.window.present();
    }

    #[allow(dead_code)]
    pub fn add_toast(&self, toast: adw::Toast) {
        self.toast_overlay.add_toast(toast);
    }

    #[allow(clippy::too_many_arguments)]
    fn build_ui(
        &self,
        app: &adw::Application,
        cmd_tx: Sender<DeviceCommand>,
        event_rx: Receiver<DeviceEvent>,
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        saved_devices: Vec<SavedDevice>,
        athlete: AthleteProfile,
        workout: Workout,
        workouts: Vec<Workout>,
    ) {
        // Restore last window size (saved on close)
        let saved_w = rt_handle
            .block_on(db::get_setting(&pool, "window.width"))
            .ok()
            .flatten()
            .and_then(|v| v.parse::<i32>().ok());
        let saved_h = rt_handle
            .block_on(db::get_setting(&pool, "window.height"))
            .ok()
            .flatten()
            .and_then(|v| v.parse::<i32>().ok());
        if let (Some(w), Some(h)) = (saved_w, saved_h) {
            self.window.set_default_size(w, h);
        }

        // The close handler is registered further down, once the flags that tell
        // us whether a ride is in progress exist.

        // ── The athlete profile: one cell, shared by every page ───────────────
        // Preferences writes here and nowhere else. Anything that needs FTP,
        // weight or HR must borrow at the point of use rather than capture a
        // copy — a captured copy silently keeps the value it had at startup, so
        // pages disagree with each other the moment the rider edits the profile.
        let athlete_rc = Rc::new(RefCell::new(athlete));

        let stack = adw::ViewStack::new();

        // ── Nav icons — bundled in the app gresource, so they always resolve
        // (see data/icons/symbolic/README.md) ─────────────────────────────────
        let calendar_icon = "calendar-symbolic";
        let fitness_icon = "graph-symbolic";
        let coaching_icon = "brain-augmented-symbolic";

        // ── Non-library pages ─────────────────────────────────────────────────
        let devices_rc = Rc::new(RefCell::new(DevicesPage::new(
            cmd_tx.clone(),
            pool.clone(),
            rt_handle.clone(),
            saved_devices,
        )));

        let mut engine =
            WorkoutEngine::new(workout.clone(), Rc::clone(&athlete_rc), cmd_tx.clone());
        engine.erg_ramp_rate = rt_handle
            .block_on(db::get_setting(&pool, "training.erg_ramp_rate"))
            .unwrap_or(None)
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(25);
        let engine_rc = Rc::new(RefCell::new(engine));
        let player_rc = Rc::new(RefCell::new(PlayerPage::new(
            &workout,
            &athlete_rc.borrow(),
        )));

        // ── Header resume button — created here so on_complete and do_start can ref it ──
        // Only visible while a workout is running and the user has navigated away
        // from the player; starting a workout always happens contextually (Today
        // hero, Library, Calendar), never from the header.
        let workout_active = Rc::new(Cell::new(false));
        let start_btn = gtk::Button::builder()
            .label("Resume Workout")
            .css_classes(["suggested-action"])
            .tooltip_text("Return to the workout in progress")
            .visible(false)
            .build();

        // Created early so connect_visible_child_notify can reference it
        let back_btn = gtk::Button::builder()
            .icon_name("go-previous-symbolic")
            .tooltip_text("Back to Dashboard")
            .css_classes(["flat", "circular"])
            .visible(false)
            .build();

        // ── Summary page — shown after workout completion ─────────────────────
        let stack_for_done = stack.clone();
        let summary_page = SummaryPage::new(move || {
            stack_for_done.set_visible_child_name("dashboard");
        });

        // ── Timer gate ────────────────────────────────────────────────────────
        // Declared here so on_complete can reset them after a workout ends.
        // timer_alive: set false to stop a running timer before starting another
        // timer_started: guards against starting the GLib timer more than once
        let timer_alive = Rc::new(Cell::new(false));
        let timer_started = Rc::new(Cell::new(false));

        // ── on_complete: save session → summary page ─────────────────────────
        let stack_for_complete = stack.clone();
        let summary_for_complete = summary_page.clone();
        let pool_for_complete = pool.clone();
        let rt_for_complete = rt_handle.clone();
        let athlete_for_complete = Rc::clone(&athlete_rc);
        let workout_active_complete = Rc::clone(&workout_active);
        let start_btn_complete = start_btn.clone();
        let engine_for_complete = Rc::clone(&engine_rc);
        let player_for_complete = Rc::clone(&player_rc);
        let timer_started_complete = Rc::clone(&timer_started);
        let timer_alive_complete = Rc::clone(&timer_alive);
        let toast_overlay_for_complete = self.toast_overlay.clone();
        // Cloned so we can present the RPE dialog over the app window from inside the closure.
        let window_for_rpe = self.window.clone();

        // Row id of the ride currently being checkpointed, or None before the
        // first checkpoint has been written. Finishing a ride overwrites that row
        // rather than inserting a second one.
        let checkpoint_id: Rc<Cell<Option<i64>>> = Rc::new(Cell::new(None));
        let checkpoint_id_complete = Rc::clone(&checkpoint_id);

        // The tail of finishing a ride — summary page, RPE, save, upload — is the
        // same whether it was a structured workout or a route ride, so both paths
        // funnel into this one closure. `segments` is None for a route ride, which
        // has no plan to compare against.
        let finish_session: FinishSession = Rc::new(move |session, name, segments| {
            // Borrowed, not captured: the profile must be whatever it is now, so
            // the summary and the FIT export it feeds carry the rider's current
            // weight and HR range rather than the values held at startup. The
            // borrow is dropped before the summary page can emit any signal.
            let athlete_now = athlete_for_complete.borrow().clone();
            // The FTP the ride was actually executed at — the stamped value wins
            // over the profile, so a bump taken mid-ride is not re-suggested.
            let ftp = session.ftp_watts.unwrap_or(athlete_now.ftp_watts);
            summary_for_complete.update(&session, &name, &athlete_now, segments.as_deref());
            stack_for_complete.set_visible_child_name("summary");

            // FTP auto-suggestion based on 20-minute best power
            if let Some(peak_20) = session.peak_power_for_duration(1200) {
                let suggested = (peak_20 as f32 * 0.95) as u32;
                if suggested > ftp + 5 {
                    toast_overlay_for_complete.add_toast(
                        adw::Toast::builder()
                            .title(format!(
                                "Your 20-min best suggests an FTP of {} W (currently {} W). \
                                 Update in Preferences.",
                                suggested, ftp
                            ))
                            .timeout(10)
                            .build(),
                    );
                }
            }

            let pool = pool_for_complete.clone();
            let workout_id = session.workout_id;

            // session_id is set by the tokio save task and read by the RPE callback.
            // The session save takes ~10 ms; the RPE dialog requires human interaction
            // (several seconds minimum), so the Arc will be populated well before the
            // user can submit — the race window is negligible.
            let session_id_arc: std::sync::Arc<std::sync::Mutex<Option<i64>>> =
                std::sync::Arc::new(std::sync::Mutex::new(None));
            let session_id_for_rpe = std::sync::Arc::clone(&session_id_arc);
            let session_id_for_task = std::sync::Arc::clone(&session_id_arc);

            // Show the RPE questionnaire immediately after the workout ends.
            let pool_rpe = pool_for_complete.clone();
            let rt_rpe = rt_for_complete.clone();
            let summary_for_rpe = summary_for_complete.clone();
            crate::ui::widgets::rpe_dialog::show(&window_for_rpe, move |rpe| {
                summary_for_rpe.show_rpe_icon(rpe);

                if let Some(sid) = *session_id_for_rpe
                    .lock()
                    .expect("session_id_arc cannot be poisoned")
                {
                    let p = pool_rpe.clone();
                    rt_rpe.spawn(async move {
                        if let Err(e) = db::save_session_rpe(&p, sid, rpe).await {
                            tracing::error!("save_session_rpe failed: {e}");
                        }
                    });
                } else {
                    tracing::warn!("RPE submitted before session was saved — RPE not persisted");
                }
            });

            // Take the checkpoint row so this ride finalises the row it has been
            // writing to all along. Cleared immediately: the ride is over, and a
            // later checkpoint must never reuse this id.
            let existing_row = checkpoint_id_complete.take();

            rt_for_complete.spawn(async move {
                match db::upsert_session(&pool, existing_row, &session).await {
                    Err(e) => {
                        tracing::error!("save_session failed: {e}");
                    }
                    Ok(session_id) => {
                        *session_id_for_task
                            .lock()
                            .expect("session_id_arc cannot be poisoned") = Some(session_id);
                        tracing::info!("Session saved");
                        if let Some(wid) = workout_id {
                            let today = chrono::Local::now().format("%Y-%m-%d").to_string();
                            if let Err(e) =
                                db::complete_today_calendar_entry(&pool, wid, &today).await
                            {
                                tracing::error!("complete_today_calendar_entry failed: {e}");
                            }
                        }
                        // Intervals.icu upload — send the full FIT file so Intervals.icu gets
                        // time-series data (power curve, HR, cadence) and can sync it to
                        // Garmin Connect with complete activity data.
                        let upload_enabled = db::get_setting(&pool, "intervals.upload")
                            .await
                            .unwrap_or(None)
                            .map(|v| v == "1")
                            .unwrap_or(false);
                        if upload_enabled {
                            let api_key = crate::data::keystore::get_secret(
                                crate::data::keystore::KEY_INTERVALS_API,
                            )
                            .unwrap_or(None)
                            .unwrap_or_default();
                            let athlete_id = db::get_setting(&pool, "intervals.athlete_id")
                                .await
                                .unwrap_or(None)
                                .unwrap_or_default();
                            if !api_key.trim().is_empty() && !athlete_id.trim().is_empty() {
                                // Loaded fresh rather than captured at startup so the
                                // export carries the FTP and heart-rate limits in
                                // force now, which is what training load is scaled to.
                                let profile =
                                    db::load_or_create_athlete(&pool).await.unwrap_or_default();
                                let fit_bytes =
                                    crate::data::fit::encode_session(&session, &profile);
                                match crate::ai::intervals::upload_fit_activity(
                                    &athlete_id,
                                    &api_key,
                                    fit_bytes,
                                    &name,
                                )
                                .await
                                {
                                    Ok(()) => {
                                        tracing::info!("Session uploaded to Intervals.icu");
                                        // Mark the session so compute_load_metrics skips its
                                        // local TSS — the same workout is now in
                                        // intervals_activities and would otherwise be counted twice.
                                        if let Err(e) =
                                            db::mark_session_uploaded_to_icu(&pool, session_id)
                                                .await
                                        {
                                            tracing::error!("mark_session_uploaded_to_icu: {e}");
                                        }
                                    }
                                    Err(e) => tracing::error!("Intervals.icu upload failed: {e}"),
                                }
                            }
                        }
                    } // Ok(session_id)
                } // match save_session
            });
        });

        // Finishing a structured workout also resets the engine and player so the
        // same workout can be started again cleanly.
        let finish_for_workout = Rc::clone(&finish_session);
        let on_complete = move |session: crate::data::session::Session| {
            workout_active_complete.set(false);
            start_btn_complete.set_visible(false);
            // Reset timer state so the next "Start Workout" click starts fresh.
            timer_alive_complete.set(false);
            timer_started_complete.set(false);
            let workout = engine_for_complete.borrow().workout.clone();
            engine_for_complete
                .borrow_mut()
                .reset_with_workout(workout.clone());
            let ftp = engine_for_complete.borrow().athlete.borrow().ftp_watts;
            player_for_complete.borrow().reset_workout(&workout, ftp);
            let name = workout.name.clone();
            let segments = workout.segments.clone();
            finish_for_workout(session, name, Some(segments));
        };

        // ── Shared "start" closure — dashboard card, library, calendar ──────
        let do_start: Rc<dyn Fn()> = {
            let stack = stack.clone();
            let player = Rc::clone(&player_rc);
            let engine = Rc::clone(&engine_rc);
            let timer_started = Rc::clone(&timer_started);
            let timer_alive = Rc::clone(&timer_alive);
            let workout_active = Rc::clone(&workout_active);
            let start_btn = start_btn.clone();
            Rc::new(move || {
                stack.set_visible_child_name("player");
                workout_active.set(true);
                if !timer_started.get() {
                    timer_started.set(true);
                    let stack_cancel = stack.clone();
                    let wa_cancel = Rc::clone(&workout_active);
                    let sb_cancel = start_btn.clone();
                    PlayerPage::start_timer(
                        Rc::clone(&player),
                        Rc::clone(&engine),
                        on_complete.clone(),
                        move || {
                            wa_cancel.set(false);
                            sb_cancel.set_visible(false);
                            stack_cancel.set_visible_child_name("dashboard");
                        },
                        Rc::clone(&timer_alive),
                    );
                }
            })
        };

        // ── Library "start a specific workout" callback ──────────────────────
        let on_library_start: Rc<dyn Fn(Workout)> = {
            let engine = Rc::clone(&engine_rc);
            let player = Rc::clone(&player_rc);
            let timer_alive = Rc::clone(&timer_alive);
            let timer_started = Rc::clone(&timer_started);
            let do_start = Rc::clone(&do_start);
            Rc::new(move |workout: Workout| {
                timer_alive.set(false); // stop any running timer on its next tick
                engine.borrow_mut().reset_with_workout(workout.clone());
                let ftp = engine.borrow().athlete.borrow().ftp_watts;
                player.borrow().reset_workout(&workout, ftp);
                timer_started.set(false);
                do_start();
            })
        };

        // ── Resume an interrupted ride ────────────────────────────────────────
        // Same path as starting a workout, except the engine is seeded with the
        // recorded session instead of an empty one, and the checkpoint row is
        // adopted so the rest of the ride keeps writing to it rather than
        // starting a second row.
        let on_resume_ride: Rc<dyn Fn(Workout, crate::data::session::Session, i64)> = {
            let engine = Rc::clone(&engine_rc);
            let player = Rc::clone(&player_rc);
            let timer_alive = Rc::clone(&timer_alive);
            let timer_started = Rc::clone(&timer_started);
            let do_start = Rc::clone(&do_start);
            let checkpoint_id = Rc::clone(&checkpoint_id);
            Rc::new(move |workout: Workout, session, row_id: i64| {
                timer_alive.set(false);
                let ftp = engine.borrow().athlete.borrow().ftp_watts;
                player.borrow().reset_workout(&workout, ftp);
                engine.borrow_mut().resume_session(workout, session);
                checkpoint_id.set(Some(row_id));
                timer_started.set(false);
                do_start();
            })
        };

        // ── Calendar page — built after on_library_start so "Load Now" can start workouts ──
        let toast_overlay_for_cal = self.toast_overlay.clone();
        let on_toast_cal: Rc<dyn Fn(adw::Toast)> =
            Rc::new(move |toast| toast_overlay_for_cal.add_toast(toast));

        let stack_for_coaching_nav = stack.clone();
        let on_go_to_coaching: Rc<dyn Fn()> =
            Rc::new(move || stack_for_coaching_nav.set_visible_child_name("coaching"));

        let (calendar_page, calendar_reload) = CalendarPage::new(
            pool.clone(),
            rt_handle.clone(),
            workouts.clone(),
            Rc::clone(&on_library_start),
            Rc::clone(&athlete_rc),
            on_toast_cal,
            on_go_to_coaching,
        );

        // ── Fitness page ──────────────────────────────────────────────────────
        let toast_overlay_for_fitness = self.toast_overlay.clone();
        let toast_fn_fitness: Rc<dyn Fn(adw::Toast)> =
            Rc::new(move |toast| toast_overlay_for_fitness.add_toast(toast));

        let (fitness_page, fitness_reload) = FitnessPage::new(
            pool.clone(),
            rt_handle.clone(),
            Rc::clone(&athlete_rc),
            toast_fn_fitness,
        );

        // ── Coaching page ─────────────────────────────────────────────────────
        let toast_overlay_for_coaching = self.toast_overlay.clone();
        let toast_fn_coaching: Rc<dyn Fn(adw::Toast)> =
            Rc::new(move |toast| toast_overlay_for_coaching.add_toast(toast));

        let (coaching_page, coaching_reload) = CoachingPage::new(
            pool.clone(),
            rt_handle.clone(),
            Rc::clone(&athlete_rc),
            workouts.clone(),
            Rc::clone(&on_library_start),
            toast_fn_coaching,
        );

        // ── Dashboard page — needs on_library_start for today's workout ────────
        let stack_for_fitness_nav = stack.clone();
        let on_view_fitness: Rc<dyn Fn()> = Rc::new(move || {
            stack_for_fitness_nav.set_visible_child_name("fitness");
        });
        let stack_for_cal_nav = stack.clone();
        let on_open_calendar: Rc<dyn Fn()> = Rc::new(move || {
            stack_for_cal_nav.set_visible_child_name("calendar");
        });
        let toast_overlay_for_dash = self.toast_overlay.clone();
        let toast_fn_dash: Rc<dyn Fn(adw::Toast)> =
            Rc::new(move |toast| toast_overlay_for_dash.add_toast(toast));

        let (dashboard_page, dashboard_reload) = DashboardPage::new(
            pool.clone(),
            rt_handle.clone(),
            Rc::clone(&athlete_rc),
            Rc::clone(&on_library_start),
            on_view_fitness,
            on_open_calendar,
            toast_fn_dash,
        );

        // ── Route player page ─────────────────────────────────────────────────
        // Built with a placeholder route; reset_route() is called before each ride.
        let blank_route = Route {
            name: String::new(),
            points: Vec::new(),
            total_distance_m: 0.0,
            total_gain_m: 0.0,
        };
        let route_player_rc = Rc::new(RoutePlayerPage::new(
            &blank_route,
            athlete_rc.borrow().ftp_watts,
        ));
        let route_timer_alive = Rc::new(Cell::new(false));
        let route_timer_started = Rc::new(Cell::new(false));

        // True while a controllable trainer (BLE FTMS or ANT+ FE-C) is connected.
        // The route player checks this each tick to pick SIM vs ERG emulation.
        let sim_capable = Rc::new(Cell::new(false));

        // SIM feel settings — read live by the ride loop so a change in
        // Preferences applies mid-ride. Difficulty is stored as a percentage
        // and held here as a 0.0–1.0 scale factor.
        let sim_difficulty = Rc::new(Cell::new(
            rt_handle
                .block_on(db::get_setting(&pool, "training.sim_difficulty"))
                .unwrap_or(None)
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(100.0)
                / 100.0,
        ));
        let sim_max_grade = Rc::new(Cell::new(
            rt_handle
                .block_on(db::get_setting(&pool, "training.sim_max_gradient"))
                .unwrap_or(None)
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(20.0),
        ));
        let trainer_addr: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

        // on_start_route: called from the library when the user clicks "Ride this Route"
        let on_start_route: Rc<dyn Fn(Route)> = {
            let route_player = Rc::clone(&route_player_rc);
            let stack = stack.clone();
            let cmd_tx = cmd_tx.clone();
            let timer_alive = Rc::clone(&route_timer_alive);
            let timer_started = Rc::clone(&route_timer_started);
            let sim_capable = Rc::clone(&sim_capable);
            let sim_difficulty = Rc::clone(&sim_difficulty);
            let sim_max_grade = Rc::clone(&sim_max_grade);
            let finish_for_route = Rc::clone(&finish_session);
            let athlete_for_route = Rc::clone(&athlete_rc);
            Rc::new(move |route: Route| {
                timer_alive.set(false);
                timer_started.set(false);
                route_player.reset_route(&route);
                stack.set_visible_child_name("route_player");

                if !timer_started.get() {
                    timer_started.set(true);
                    let route_name = route.name.clone();
                    let finish = Rc::clone(&finish_for_route);
                    // Cloned out so no borrow is held while start_timer installs
                    // its callbacks — see CLAUDE.md §2.4.
                    let athlete_now = athlete_for_route.borrow().clone();
                    RoutePlayerPage::start_timer(
                        Rc::clone(&route_player),
                        route,
                        &athlete_now,
                        cmd_tx.clone(),
                        Rc::clone(&sim_capable),
                        Rc::clone(&sim_difficulty),
                        Rc::clone(&sim_max_grade),
                        move |session| {
                            // A route ride ends the same way a workout does: summary
                            // page, RPE, save and upload. It has no plan to compare
                            // against, so it passes no segments.
                            finish(session, route_name.clone(), None);
                        },
                        Rc::clone(&timer_alive),
                    );
                }
            })
        };

        // ── Library page — needs on_library_start, so built after it ────────
        let toast_overlay_for_lib = self.toast_overlay.clone();
        let on_toast_lib: Rc<dyn Fn(adw::Toast)> =
            Rc::new(move |toast| toast_overlay_for_lib.add_toast(toast));
        let (library_page, library_reload) = LibraryPage::new(
            workouts,
            pool.clone(),
            rt_handle.clone(),
            Rc::clone(&on_library_start),
            calendar_icon,
            on_toast_lib,
            Rc::clone(&athlete_rc),
            on_start_route,
        );
        let library_reload = Rc::new(library_reload);
        // The dashboard has already loaded by the time the recovery prompt is
        // answered, so recovery refreshes it explicitly; every other page reloads
        // on navigation anyway.
        let dashboard_reload_recover = Rc::clone(&dashboard_reload);

        // ── Add all pages to stack ────────────────────────────────────────────
        stack.add_named(dashboard_page.widget(), Some("dashboard"));
        stack.add_named(calendar_page.widget(), Some("calendar"));
        stack.add_named(library_page.widget(), Some("library"));
        stack.add_named(devices_rc.borrow().widget(), Some("devices"));
        stack.add_named(player_rc.borrow().widget(), Some("player"));
        stack.add_named(route_player_rc.widget(), Some("route_player"));
        stack.add_named(summary_page.widget(), Some("summary"));
        stack.add_named(fitness_page.widget(), Some("fitness"));
        stack.add_named(coaching_page.widget(), Some("coaching"));

        stack.set_visible_child_name("dashboard");

        // ── Sidebar list ─────────────────────────────────────────────────────
        let sidebar_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .css_classes(["navigation-sidebar"])
            .build();

        let nav_items = [
            ("Dashboard", "go-home-symbolic", "dashboard"),
            ("Calendar", calendar_icon, "calendar"),
            ("Library", "folder-open-symbolic", "library"),
            ("Fitness", fitness_icon, "fitness"),
            ("Coaching", coaching_icon, "coaching"),
            ("Devices", "bluetooth-symbolic", "devices"),
        ];

        for (label, icon, page_name) in &nav_items {
            let row = Self::make_nav_row(label, icon, page_name);
            sidebar_list.append(&row);
        }

        if let Some(first_row) = sidebar_list.row_at_index(0) {
            sidebar_list.select_row(Some(&first_row));
        }

        let stack_clone = stack.clone();
        sidebar_list.connect_row_selected(move |_, row| {
            if let Some(row) = row {
                let page_name = row.widget_name();
                let page_name = page_name.as_str();
                if stack_clone.child_by_name(page_name).is_some() {
                    stack_clone.set_visible_child_name(page_name);
                }
            }
        });

        // ── Content NavigationPage — created early so the title-update closure can capture it.
        // Child (content_box) is set after the header bar and stack are assembled below.
        let content_nav_page = adw::NavigationPage::builder()
            .title("Dashboard")
            .tag("content")
            .build();

        // Reload data pages whenever they become visible (sidebar nav or
        // programmatic switches such as "Back to Dashboard" from summary).
        // Also update the content navigation page title per GNOME HIG.
        // Also show/hide the Resume Workout button: it only appears while a
        // workout is running and the user is away from the player page.
        let content_nav_for_title = content_nav_page.clone();
        let start_btn_for_vis = start_btn.clone();
        let workout_active_for_vis = Rc::clone(&workout_active);
        let back_btn_for_vis = back_btn.clone();
        stack.connect_visible_child_notify(move |s| {
            let page = s.visible_child_name();
            let title = match page.as_deref() {
                Some("dashboard") | None => "Dashboard",
                Some("calendar") => "Calendar",
                Some("library") => "Library",
                Some("fitness") => "Fitness",
                Some("coaching") => "Coaching",
                Some("devices") => "Devices",
                Some("player") => "Workout",
                Some("route_player") => "Route Ride",
                Some("summary") => "Summary",
                _ => "Cycle",
            };
            content_nav_for_title.set_title(title);
            let show_start =
                workout_active_for_vis.get() && !matches!(page.as_deref(), Some("player"));
            start_btn_for_vis.set_visible(show_start);
            let show_back = matches!(page.as_deref(), Some("summary"));
            back_btn_for_vis.set_visible(show_back);
            match page.as_deref() {
                Some("dashboard") => dashboard_reload(),
                Some("calendar") => calendar_reload(),
                Some("library") => library_reload(),
                Some("fitness") => fitness_reload(),
                Some("coaching") => coaching_reload(),
                _ => {}
            }
        });

        // ── Sidebar chrome ───────────────────────────────────────────────────
        let sidebar_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();

        let sidebar_header = adw::HeaderBar::builder()
            .show_end_title_buttons(false)
            .build();
        sidebar_box.append(&sidebar_header);

        let sidebar_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&sidebar_list)
            .build();
        sidebar_box.append(&sidebar_scroll);

        let sidebar_nav_page = adw::NavigationPage::builder()
            .title("Cycle")
            .tag("sidebar")
            .child(&sidebar_box)
            .build();

        // ── Content chrome ───────────────────────────────────────────────────
        let content_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();

        let content_header = adw::HeaderBar::new();

        let menu_button = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .tooltip_text("Main Menu")
            .build();
        let main_menu = gio::Menu::new();
        main_menu.append(Some("Preferences"), Some("app.preferences"));
        main_menu.append(Some("About Cycle"), Some("app.about"));
        menu_button.set_menu_model(Some(&main_menu));
        content_header.pack_end(&menu_button);

        let fs_exit_btn = gtk::Button::builder()
            .icon_name("view-restore-symbolic")
            .tooltip_text("Exit Fullscreen")
            .css_classes(["flat", "circular"])
            .visible(false)
            .build();
        content_header.pack_end(&fs_exit_btn);

        content_header.pack_start(&back_btn);
        content_header.pack_start(&start_btn);

        // Back button: navigate to dashboard from summary page
        let stack_for_back = stack.clone();
        back_btn.connect_clicked(move |_| {
            stack_for_back.set_visible_child_name("dashboard");
        });

        // Return to the active player. If the workout is paused, this also
        // resumes it — the button reads "Resume Workout" and users expect that.
        let stack_for_btn = stack.clone();
        let engine_for_btn = Rc::clone(&engine_rc);
        let player_for_btn = Rc::clone(&player_rc);
        start_btn.connect_clicked(move |_| {
            if engine_for_btn.borrow().state == EngineState::Paused {
                player_for_btn.borrow().trigger_pause_toggle();
            }
            stack_for_btn.set_visible_child_name("player");
        });

        content_box.append(&content_header);
        content_box.append(&stack);

        // Set the child now that content_box is fully assembled
        content_nav_page.set_child(Some(&content_box));

        let split_view = adw::NavigationSplitView::builder()
            .sidebar(&sidebar_nav_page)
            .content(&content_nav_page)
            .sidebar_width_fraction(0.22)
            .min_sidebar_width(200.0)
            .max_sidebar_width(280.0)
            .build();

        // ── Fullscreen support ─────────────────────────────────────────────────
        let fs_btn_notify = fs_exit_btn.clone();
        self.window.connect_fullscreened_notify(move |win| {
            fs_btn_notify.set_visible(win.is_fullscreen());
        });

        let window_unfull = self.window.clone();
        fs_exit_btn.connect_clicked(move |_| {
            window_unfull.unfullscreen();
        });

        self.toast_overlay.set_child(Some(&split_view));
        self.window.set_content(Some(&self.toast_overlay));

        let window_for_key = self.window.clone();
        let key_ctrl = gtk::EventControllerKey::new();
        key_ctrl.connect_key_pressed(move |_, key, _, _| {
            if key == gtk::gdk::Key::F11 {
                if window_for_key.is_fullscreen() {
                    window_for_key.unfullscreen();
                } else {
                    window_for_key.fullscreen();
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.window.add_controller(key_ctrl);

        // Standard GNOME keyboard shortcuts
        app.set_accels_for_action("app.quit", &["<Control>q"]);
        let close_action = gio::SimpleAction::new("close", None);
        let window_for_close_action = self.window.clone();
        close_action.connect_activate(move |_, _| window_for_close_action.close());
        self.window.add_action(&close_action);
        app.set_accels_for_action("win.close", &["<Control>w"]);

        // ── App actions ──────────────────────────────────────────────────────
        let window_for_about = self.window.clone();
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
        let window_for_prefs = self.window.clone();
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

        // ── Mid-ride checkpointing ────────────────────────────────────────────
        // A ride otherwise lives only in memory until it ends, so a crash, a
        // suspend or a lost session takes the whole thing. Write the ride so far
        // to its own row every 30 s; the row carries a NULL ended_at until the
        // ride finishes, which is what marks it recoverable at startup.
        //
        // Runs on the GTK main thread (it reads the engine and the route page),
        // but only clones the session there — the write itself goes to tokio.
        let checkpoint_engine = Rc::clone(&engine_rc);
        let checkpoint_route = Rc::clone(&route_player_rc);
        let checkpoint_workout_active = Rc::clone(&workout_active);
        let checkpoint_route_active = Rc::clone(&route_timer_alive);
        let checkpoint_pool = pool.clone();
        let checkpoint_rt = rt_handle.clone();
        let checkpoint_id_timer = Rc::clone(&checkpoint_id);
        glib::timeout_add_local(Duration::from_secs(CHECKPOINT_INTERVAL_SECS), move || {
            let snapshot = if checkpoint_workout_active.get() {
                let engine = checkpoint_engine.borrow();
                let session = engine.session.clone();
                drop(engine);
                Some(session)
            } else if checkpoint_route_active.get() {
                checkpoint_route.live_session_snapshot()
            } else {
                None
            };

            // Nothing worth writing until the rider has actually produced data —
            // this keeps an opened-but-unridden workout out of the recovery list.
            let Some(session) = snapshot.filter(|s| !s.data_points.is_empty()) else {
                return glib::ControlFlow::Continue;
            };

            let existing = checkpoint_id_timer.get();
            let pool = checkpoint_pool.clone();
            let cleanup_pool = checkpoint_pool.clone();
            let cleanup_rt = checkpoint_rt.clone();
            let id_cell = Rc::clone(&checkpoint_id_timer);
            let wa = Rc::clone(&checkpoint_workout_active);
            let ra = Rc::clone(&checkpoint_route_active);
            crate::ui::spawn_to_main(
                &checkpoint_rt,
                async move { db::checkpoint_session(&pool, existing, &session).await },
                move |result| match result {
                    Ok(row_id) => {
                        if wa.get() || ra.get() {
                            id_cell.set(Some(row_id));
                        } else if existing.is_none() {
                            // The ride finished while this first checkpoint was in
                            // flight, so it wrote its own row and this one is a
                            // duplicate that would surface as a phantom recovery.
                            cleanup_rt.spawn(async move {
                                if let Err(e) = db::delete_session(&cleanup_pool, row_id).await {
                                    tracing::error!("stale checkpoint cleanup failed: {e}");
                                }
                            });
                        }
                    }
                    Err(e) => tracing::error!("checkpoint failed: {e}"),
                },
            );
            glib::ControlFlow::Continue
        });

        // ── Window close ──────────────────────────────────────────────────────
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
        self.window.connect_close_request(move |win| {
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

        // ── GLib event polling loop ───────────────────────────────────────────
        let player_for_loop = Rc::clone(&player_rc);
        let route_player_for_loop = Rc::clone(&route_player_rc);
        let devices_for_loop = Rc::clone(&devices_rc);
        let toast_overlay_for_loop = self.toast_overlay.clone();
        glib::timeout_add_local(Duration::from_millis(50), move || {
            while let Ok(event) = event_rx.try_recv() {
                match event {
                    DeviceEvent::Readings(readings) => {
                        player_for_loop.borrow().set_readings(readings.clone());
                        route_player_for_loop.set_readings(readings);
                    }
                    DeviceEvent::PeripheralDiscovered {
                        address,
                        name,
                        rssi,
                        transport,
                        kind,
                    } => {
                        devices_for_loop
                            .borrow_mut()
                            .on_discovered(address, name, rssi, transport, kind);
                    }
                    DeviceEvent::ConnectionChanged {
                        address,
                        connected,
                        device_type,
                    } => {
                        let display_name = devices_for_loop.borrow().display_name_for(&address);
                        devices_for_loop.borrow_mut().on_connection_changed(
                            address.clone(),
                            connected,
                            device_type,
                        );
                        // Track whether a controllable trainer is available for SIM mode.
                        if connected && device_type == Some(DeviceType::FtmsTrainer) {
                            *trainer_addr.borrow_mut() = Some(address.clone());
                            sim_capable.set(true);
                        } else if !connected
                            && trainer_addr.borrow().as_deref() == Some(address.as_str())
                        {
                            *trainer_addr.borrow_mut() = None;
                            sim_capable.set(false);
                        }
                        if connected {
                            player_for_loop
                                .borrow()
                                .add_connected_device(&address, &display_name);
                            route_player_for_loop.add_connected_device(&address, &display_name);
                            toast_overlay_for_loop.add_toast(
                                adw::Toast::builder()
                                    .title(format!("Connected: {}", display_name))
                                    .timeout(4)
                                    .build(),
                            );
                        } else {
                            player_for_loop.borrow().remove_connected_device(&address);
                            route_player_for_loop.remove_connected_device(&address);
                        }
                    }
                    DeviceEvent::Error(e) => {
                        tracing::error!("Device error: {e}");
                    }
                    DeviceEvent::Warning(msg) => {
                        tracing::warn!("Device warning: {msg}");
                        toast_overlay_for_loop
                            .add_toast(adw::Toast::builder().title(msg).timeout(5).build());
                    }
                }
            }
            glib::ControlFlow::Continue
        });

        // ── Recover interrupted rides ─────────────────────────────────────────
        // Checkpointed rows whose ended_at is still NULL are rides the app never
        // got to finish. Runs through spawn_to_main, so the callback lands on the
        // main loop once the window is up.
        let recover_pool = pool.clone();
        let recover_rt = rt_handle.clone();
        let recover_window = self.window.clone();
        let recover_reload = dashboard_reload_recover;
        let recover_resume = Rc::clone(&on_resume_ride);
        crate::ui::spawn_to_main(
            &rt_handle,
            {
                let pool = recover_pool.clone();
                async move {
                    let records = db::load_unfinished_sessions(&pool).await?;
                    // Pair each ride with its plan. Resuming needs the workout, and
                    // it may have been deleted from the library since — in which
                    // case the ride can still be kept, just not continued.
                    let mut paired = Vec::with_capacity(records.len());
                    for record in records {
                        let workout = match record.session.workout_id {
                            Some(id) => db::load_workout_by_id(&pool, id).await.unwrap_or(None),
                            None => None,
                        };
                        paired.push((record, workout));
                    }
                    Ok::<_, anyhow::Error>(paired)
                }
            },
            move |result| {
                let paired = match result {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::error!("could not check for interrupted rides: {e}");
                        return;
                    }
                };
                for (record, workout) in paired {
                    Self::offer_recovery(
                        &recover_window,
                        recover_pool.clone(),
                        recover_rt.clone(),
                        record,
                        workout,
                        Rc::clone(&recover_reload),
                        Rc::clone(&recover_resume),
                    );
                }
            },
        );
    }

    /// Ask what to do with a ride the app never finished writing.
    ///
    /// Continuing puts the rider back on the workout screen at the second the
    /// ride stopped, behind the usual ten-second power gate. Keeping it instead
    /// files the ride as it stands, stamping an end time derived from the last
    /// recorded second so its duration reflects what was actually ridden rather
    /// than the gap until the app was reopened.
    ///
    /// `workout` is the plan the ride was following, and is `None` for a route
    /// ride or one whose workout has since been deleted — continuing needs the
    /// plan, so it is only offered when there is one.
    #[allow(clippy::too_many_arguments)]
    fn offer_recovery(
        window: &adw::ApplicationWindow,
        pool: SqlitePool,
        rt: tokio::runtime::Handle,
        record: db::SessionRecord,
        workout: Option<Workout>,
        reload: Rc<dyn Fn()>,
        on_resume: Rc<dyn Fn(Workout, crate::data::session::Session, i64)>,
    ) {
        let session = record.session;
        let ridden_secs = session
            .data_points
            .last()
            .map(|p| p.elapsed_secs)
            .unwrap_or(0);
        // A checkpoint is only written once there is data, so an empty row means
        // something went wrong rather than a ride worth offering back.
        if ridden_secs == 0 {
            let pool_empty = pool.clone();
            rt.spawn(async move {
                let _ = db::delete_session(&pool_empty, session.id).await;
            });
            return;
        }

        let when = session
            .started_at
            .with_timezone(&chrono::Local)
            .format("%A %-d %B, %H:%M");
        let name = record.workout_name.as_deref().unwrap_or("Route ride");
        let ridden = crate::training::engine::WorkoutEngine::format_duration(ridden_secs);
        let body = match &workout {
            Some(w) => {
                let remaining = w.duration_secs.saturating_sub(ridden_secs);
                format!(
                    "“{name}” from {when} was still recording when Cycle closed.\n\n\
                     {ridden} was saved, with {} still to ride. Continuing puts you \
                     back on the workout with ten seconds to get going.",
                    crate::training::engine::WorkoutEngine::format_duration(remaining)
                )
            }
            None => format!(
                "“{name}” from {when} was still recording when Cycle closed.\n\n\
                 {ridden} of riding was saved."
            ),
        };
        let dialog = adw::AlertDialog::builder()
            .heading("Recover the interrupted ride?")
            .body(body)
            .build();
        dialog.add_response("discard", "_Discard");
        dialog.add_response("keep", "_Keep Ride");
        dialog.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
        // Continuing is only possible with the plan in hand, and there is nothing
        // left to ride once the workout has run its full length.
        let can_continue = workout
            .as_ref()
            .is_some_and(|w| w.duration_secs > ridden_secs);
        if can_continue {
            dialog.add_response("continue", "_Continue Ride");
            dialog.set_response_appearance("continue", adw::ResponseAppearance::Suggested);
            dialog.set_default_response(Some("continue"));
        } else {
            dialog.set_response_appearance("keep", adw::ResponseAppearance::Suggested);
            dialog.set_default_response(Some("keep"));
        }
        // Dismissing without choosing leaves the row untouched, so the offer
        // comes back next launch rather than silently resolving either way.
        dialog.set_close_response("later");

        let session_id = session.id;
        let ended_at =
            (session.started_at + chrono::Duration::seconds(ridden_secs as i64)).to_rfc3339();
        let resume_session = session.clone();
        dialog.connect_response(None, move |_, response| {
            let pool = pool.clone();
            let reload = Rc::clone(&reload);
            match response {
                "continue" => {
                    // The row stays unfinished and is handed to the ride, which
                    // keeps checkpointing to it and finalises it when it ends.
                    if let Some(w) = workout.clone() {
                        on_resume(w, resume_session.clone(), session_id);
                    }
                }
                "keep" => {
                    let ended_at = ended_at.clone();
                    crate::ui::spawn_to_main(
                        &rt,
                        async move { db::finalise_session(&pool, session_id, &ended_at).await },
                        move |r| match r {
                            Ok(()) => reload(),
                            Err(e) => tracing::error!("recovering ride failed: {e}"),
                        },
                    );
                }
                "discard" => {
                    rt.spawn(async move {
                        if let Err(e) = db::delete_session(&pool, session_id).await {
                            tracing::error!("discarding interrupted ride failed: {e}");
                        }
                    });
                }
                _ => {}
            }
        });
        dialog.present(Some(window));
    }

    fn make_nav_row(label: &str, icon_name: &str, page_name: &str) -> gtk::ListBoxRow {
        let row_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(6)
            .margin_end(6)
            .build();

        let icon = gtk::Image::builder()
            .icon_name(icon_name)
            .icon_size(gtk::IconSize::Normal)
            .build();

        let text = gtk::Label::builder()
            .label(label)
            .halign(gtk::Align::Start)
            .hexpand(true)
            .build();

        row_box.append(&icon);
        row_box.append(&text);

        gtk::ListBoxRow::builder()
            .child(&row_box)
            .name(page_name)
            .build()
    }
}
