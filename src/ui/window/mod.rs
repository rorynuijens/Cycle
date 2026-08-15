mod actions;
mod awake;
mod chrome;
mod events;
mod session;

use adw::prelude::*;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use async_channel::{Receiver, Sender};
use sqlx::SqlitePool;

use super::pages::{
    calendar::CalendarPage, coaching::CoachingPage, dashboard::DashboardPage, devices::DevicesPage,
    fitness::FitnessPage, library::LibraryPage, player::PlayerPage, route_player::RoutePlayerPage,
    summary::SummaryPage,
};
use crate::data::settings::{self, TrainingSettings, WindowSettings};
use crate::data::{
    athlete::AthleteProfile,
    db::{self, SavedDevice},
    route::Route,
    workout::Workout,
};
use crate::devices::manager::{DeviceCommand, DeviceEvent};
use crate::training::engine::WorkoutEngine;

/// Ends a ride: summary page, RPE prompt, save and upload. Takes the finished
/// session, the name to file it under, and the workout plan it was ridden
/// against (`None` for a route ride).
type FinishSession =
    Rc<dyn Fn(crate::data::session::Session, String, Option<Vec<crate::data::workout::Segment>>)>;

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
        // Restore last window size (saved on close). A read that fails is not
        // the same as a size never recorded, so say so rather than letting an
        // unreadable database look like a first run.
        let saved = rt_handle
            .block_on(settings::load_window(&pool))
            .unwrap_or_else(|e| {
                tracing::error!("Could not read the saved window size: {e}");
                WindowSettings::default()
            });
        if let Some((w, h)) = saved.size() {
            self.window.set_default_size(w, h);
        }

        // Read once, here, and used again by the SIM setup further down. Both
        // used to read the same keys separately, each with its own copy of the
        // defaults — which agreed only by coincidence.
        let training = rt_handle
            .block_on(settings::load_training(&pool))
            .unwrap_or_else(|e| {
                tracing::error!("Could not read your training settings: {e}");
                TrainingSettings::default()
            });

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
        engine.erg_ramp_rate = training.erg_ramp_rate;
        let engine_rc = Rc::new(RefCell::new(engine));
        let player_rc = Rc::new(RefCell::new(PlayerPage::new(
            &workout,
            &athlete_rc.borrow(),
        )));
        player_rc.borrow().set_cues_enabled(training.interval_cues);

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
        // ── Finishing a ride ─────────────────────────────────────────────────
        // The tail of finishing — summary, RPE, save, upload — is the same for a
        // structured workout and a route ride, so both funnel into one closure.
        let checkpoint_id: Rc<Cell<Option<i64>>> = Rc::new(Cell::new(None));
        let finish_session: FinishSession = session::finish_session_closure(
            pool.clone(),
            rt_handle.clone(),
            self.window.clone(),
            self.toast_overlay.clone(),
            stack.clone(),
            summary_page.clone(),
            Rc::clone(&athlete_rc),
            Rc::clone(&checkpoint_id),
        );

        // Finishing a structured workout also resets the engine and player so the
        // same workout can be started again cleanly.
        let workout_active_complete = Rc::clone(&workout_active);
        let start_btn_complete = start_btn.clone();
        let engine_for_complete = Rc::clone(&engine_rc);
        let player_for_complete = Rc::clone(&player_rc);
        let timer_started_complete = Rc::clone(&timer_started);
        let timer_alive_complete = Rc::clone(&timer_alive);
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
            let pool_for_cues = pool.clone();
            let rt_for_cues = rt_handle.clone();
            Rc::new(move |workout: Workout| {
                timer_alive.set(false); // stop any running timer on its next tick
                engine.borrow_mut().reset_with_workout(workout.clone());
                let ftp = engine.borrow().athlete.borrow().ftp_watts;
                player.borrow().reset_workout(&workout, ftp);
                crate::ui::pages::player::load_cues(
                    Rc::clone(&player),
                    workout,
                    pool_for_cues.clone(),
                    &rt_for_cues,
                );
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
            let pool_for_cues = pool.clone();
            let rt_for_cues = rt_handle.clone();
            Rc::new(move |workout: Workout, session, row_id: i64| {
                timer_alive.set(false);
                let ftp = engine.borrow().athlete.borrow().ftp_watts;
                player.borrow().reset_workout(&workout, ftp);
                crate::ui::pages::player::load_cues(
                    Rc::clone(&player),
                    workout.clone(),
                    pool_for_cues.clone(),
                    &rt_for_cues,
                );
                engine.borrow_mut().resume_session(workout, session);
                checkpoint_id.set(Some(row_id));
                timer_started.set(false);
                do_start();
            })
        };

        // ── The daily brief ───────────────────────────────────────────────────
        // Built before the pages that show it, so each can subscribe as it is
        // constructed. Nothing is requested until `start()` below, once every
        // page exists — otherwise a card built late would miss the result.
        let brief_store = crate::ui::brief_store::BriefStore::new(
            pool.clone(),
            rt_handle.clone(),
            Rc::clone(&athlete_rc),
        );

        // ── Calendar page — built after on_library_start so "Load Now" can start workouts ──
        let toast_overlay_for_cal = self.toast_overlay.clone();
        let on_toast_cal: Rc<dyn Fn(adw::Toast)> =
            Rc::new(move |toast| toast_overlay_for_cal.add_toast(toast));

        let stack_for_coaching_nav = stack.clone();
        let on_go_to_coaching: Rc<dyn Fn()> =
            Rc::new(move || stack_for_coaching_nav.set_visible_child_name("coaching"));
        let on_go_to_coaching_dash = Rc::clone(&on_go_to_coaching);

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
            Rc::clone(&brief_store),
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
            Rc::clone(&brief_store),
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
            Rc::clone(&brief_store),
            on_go_to_coaching_dash,
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
        let sim_difficulty = Rc::new(Cell::new(training.sim_difficulty_pct / 100.0));
        let sim_max_grade = Rc::new(Cell::new(training.sim_max_gradient_pct));
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

        // ── Sidebar ──────────────────────────────────────────────────────────
        let nav_items = [
            ("Dashboard", "go-home-symbolic", "dashboard"),
            ("Calendar", calendar_icon, "calendar"),
            ("Library", "folder-open-symbolic", "library"),
            ("Fitness", fitness_icon, "fitness"),
            ("Coaching", coaching_icon, "coaching"),
            ("Devices", "bluetooth-symbolic", "devices"),
        ];
        let sidebar_list = chrome::build_sidebar_list(&stack, &nav_items);

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
        let brief_store_for_nav = Rc::clone(&brief_store);
        let start_btn_for_vis = start_btn.clone();
        let workout_active_for_vis = Rc::clone(&workout_active);
        let route_alive_for_vis = Rc::clone(&route_timer_alive);
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
            // The way back to whichever ride is running. A route ride counts as
            // much as a workout: leaving one used to strand it with nothing on
            // screen pointing at it, and no way to reach it again.
            let workout_away =
                workout_active_for_vis.get() && !matches!(page.as_deref(), Some("player"));
            let route_away =
                route_alive_for_vis.get() && !matches!(page.as_deref(), Some("route_player"));
            if route_away {
                start_btn_for_vis.set_label("Back to Ride");
                start_btn_for_vis.set_tooltip_text(Some("Return to the route ride in progress"));
            } else if workout_away {
                start_btn_for_vis.set_label("Resume Workout");
                start_btn_for_vis.set_tooltip_text(Some("Return to the workout in progress"));
            }
            start_btn_for_vis.set_visible(workout_away || route_away);
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
            // Check whether the brief has been overtaken — a read, never a
            // request. Landing on any page that shows one is the moment the
            // rider would notice it saying something the data no longer says.
            if matches!(
                page.as_deref(),
                Some("dashboard") | Some("fitness") | Some("coaching")
            ) {
                brief_store_for_nav.revalidate();
            }
        });

        let sidebar_nav_page = chrome::build_sidebar_page(&sidebar_list);

        // ── Content chrome ───────────────────────────────────────────────────
        let content_chrome = chrome::build_content(
            &content_nav_page,
            &sidebar_nav_page,
            &stack,
            &back_btn,
            &start_btn,
            Rc::clone(&route_timer_alive),
            Rc::clone(&engine_rc),
            Rc::clone(&player_rc),
        );

        // ── Fullscreen ───────────────────────────────────────────────────────
        chrome::connect_fullscreen(
            &self.window,
            &content_chrome,
            Rc::clone(&player_rc),
            Rc::clone(&route_player_rc),
        );

        // ── Keep the screen lit for the whole ride ───────────────────────────
        awake::keep_awake_during_rides(
            app,
            &self.window,
            Rc::clone(&workout_active),
            Rc::clone(&route_timer_alive),
        );

        self.toast_overlay.set_child(Some(&content_chrome.root));
        self.window.set_content(Some(&self.toast_overlay));

        // ── The one automatic AI request the app makes ────────────────────────
        // Every page that shows a slice of the brief has now been built and has
        // subscribed, so whatever comes back reaches all of them. Whether this
        // actually asks anything is `brief::startup_action`'s decision: it only
        // does so when nothing is cached for today.
        brief_store.start();

        // ── App actions and shortcuts ────────────────────────────────────────
        actions::install(
            app,
            &self.window,
            pool.clone(),
            rt_handle.clone(),
            Rc::clone(&athlete_rc),
            Rc::clone(&engine_rc),
            Rc::clone(&player_rc),
            Rc::clone(&sim_difficulty),
            Rc::clone(&sim_max_grade),
        );

        // ── Mid-ride checkpointing ───────────────────────────────────────────
        session::start_checkpoint_timer(
            pool.clone(),
            rt_handle.clone(),
            Rc::clone(&engine_rc),
            Rc::clone(&route_player_rc),
            Rc::clone(&workout_active),
            Rc::clone(&route_timer_alive),
            Rc::clone(&checkpoint_id),
        );

        // ── Window close ─────────────────────────────────────────────────────
        actions::connect_close(
            &self.window,
            pool.clone(),
            rt_handle.clone(),
            Rc::clone(&workout_active),
            Rc::clone(&route_timer_alive),
        );

        // ── Device events ────────────────────────────────────────────────────
        events::start_polling(
            event_rx,
            Rc::clone(&player_rc),
            Rc::clone(&route_player_rc),
            Rc::clone(&devices_rc),
            self.toast_overlay.clone(),
            trainer_addr,
            sim_capable,
        );

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
                    session::offer_recovery(
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
}
