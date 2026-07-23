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
use crate::devices::manager::{DeviceCommand, DeviceEvent};
use crate::training::engine::{EngineState, WorkoutEngine};

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

        let pool_close = pool.clone();
        let rt_close = rt_handle.clone();
        self.window.connect_close_request(move |win| {
            let width = win.width();
            let height = win.height();
            let p = pool_close.clone();
            rt_close.spawn(async move {
                let _ = db::set_setting(&p, "window.width", &width.to_string()).await;
                let _ = db::set_setting(&p, "window.height", &height.to_string()).await;
            });
            glib::Propagation::Proceed
        });

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

        let mut engine = WorkoutEngine::new(workout.clone(), athlete.clone(), cmd_tx.clone());
        engine.erg_ramp_rate = rt_handle
            .block_on(db::get_setting(&pool, "training.erg_ramp_rate"))
            .unwrap_or(None)
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(25);
        let engine_rc = Rc::new(RefCell::new(engine));
        let player_rc = Rc::new(RefCell::new(PlayerPage::new(&workout, &athlete)));

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
        let ftp_initial = athlete.ftp_watts;
        let workout_active_complete = Rc::clone(&workout_active);
        let start_btn_complete = start_btn.clone();
        let engine_for_complete = Rc::clone(&engine_rc);
        let player_for_complete = Rc::clone(&player_rc);
        let timer_started_complete = Rc::clone(&timer_started);
        let timer_alive_complete = Rc::clone(&timer_alive);
        let toast_overlay_for_complete = self.toast_overlay.clone();
        // Cloned so we can present the RPE dialog over the app window from inside the closure.
        let window_for_rpe = self.window.clone();

        let on_complete = move |session: crate::data::session::Session| {
            workout_active_complete.set(false);
            start_btn_complete.set_visible(false);
            // Reset timer state so the next "Start Workout" click starts fresh.
            timer_alive_complete.set(false);
            timer_started_complete.set(false);
            // Reset engine and player so the same workout can be started again cleanly.
            let workout = engine_for_complete.borrow().workout.clone();
            engine_for_complete
                .borrow_mut()
                .reset_with_workout(workout.clone());
            player_for_complete.borrow().reset_workout(&workout);
            let name = engine_for_complete.borrow().workout.name.clone();
            let segments = engine_for_complete.borrow().workout.segments.clone();
            let ftp = ftp_initial;
            summary_for_complete.update(&session, &name, ftp, Some(&segments));
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

            rt_for_complete.spawn(async move {
                match db::save_session(&pool, &session).await {
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
                                let fit_bytes = crate::data::fit::encode_session(&session);
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
                player.borrow().reset_workout(&workout);
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
            athlete.ftp_watts,
            athlete.weight_kg,
            on_toast_cal,
            on_go_to_coaching,
        );

        // ── Shared athlete ref — used by fitness + coaching pages ─────────────
        let ftp_for_fitness = Rc::new(Cell::new(athlete.ftp_watts));
        let athlete_rc = Rc::new(RefCell::new(athlete.clone()));

        // ── Fitness page ──────────────────────────────────────────────────────
        let (fitness_page, fitness_reload) = FitnessPage::new(
            pool.clone(),
            rt_handle.clone(),
            Rc::clone(&ftp_for_fitness),
            Rc::clone(&athlete_rc),
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
        let (dashboard_page, dashboard_reload) = DashboardPage::new(
            pool.clone(),
            rt_handle.clone(),
            athlete.clone(),
            Rc::clone(&on_library_start),
            on_view_fitness,
            on_open_calendar,
        );

        // ── Route player page ─────────────────────────────────────────────────
        // Built with a placeholder route; reset_route() is called before each ride.
        let blank_route = Route {
            name: String::new(),
            points: Vec::new(),
            total_distance_m: 0.0,
            total_gain_m: 0.0,
        };
        let route_player_rc = Rc::new(RoutePlayerPage::new(&blank_route));
        let route_timer_alive = Rc::new(Cell::new(false));
        let route_timer_started = Rc::new(Cell::new(false));

        // on_start_route: called from the library when the user clicks "Ride this Route"
        let on_start_route: Rc<dyn Fn(Route)> = {
            let route_player = Rc::clone(&route_player_rc);
            let stack = stack.clone();
            let cmd_tx = cmd_tx.clone();
            let pool_route = pool.clone();
            let rt_route = rt_handle.clone();
            let toast_route = self.toast_overlay.clone();
            let timer_alive = Rc::clone(&route_timer_alive);
            let timer_started = Rc::clone(&route_timer_started);
            let mass_kg = athlete.weight_kg;
            Rc::new(move |route: Route| {
                timer_alive.set(false);
                timer_started.set(false);
                route_player.reset_route(&route);
                stack.set_visible_child_name("route_player");

                if !timer_started.get() {
                    timer_started.set(true);
                    let pool_c = pool_route.clone();
                    let rt_c = rt_route.clone();
                    let toast_c = toast_route.clone();
                    let stack_done = stack.clone();
                    RoutePlayerPage::start_timer(
                        Rc::clone(&route_player),
                        route,
                        mass_kg,
                        cmd_tx.clone(),
                        move |session| {
                            stack_done.set_visible_child_name("dashboard");
                            let pool_save = pool_c.clone();
                            rt_c.spawn(async move {
                                if let Err(e) = db::save_session(&pool_save, &session).await {
                                    tracing::error!("save route session failed: {e}");
                                } else {
                                    tracing::info!("Route session saved");
                                }
                            });
                            toast_c.add_toast(
                                adw::Toast::builder()
                                    .title("Route ride saved")
                                    .timeout(4)
                                    .build(),
                            );
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
            athlete.ftp_watts,
            on_start_route,
        );
        let library_reload = Rc::new(library_reload);

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
        let prefs_action = gio::SimpleAction::new("preferences", None);
        prefs_action.connect_activate(move |_, _| {
            let current_athlete = engine_for_prefs.borrow().athlete.clone();
            let engine_on_save = Rc::clone(&engine_for_prefs);
            let ftp_cell = Rc::clone(&ftp_for_fitness);
            let athlete_for_pages = Rc::clone(&athlete_rc);
            let engine_for_erg = Rc::clone(&engine_for_prefs);
            crate::ui::preferences::show(
                &window_for_prefs,
                current_athlete,
                pool_for_prefs.clone(),
                rt_for_prefs.clone(),
                move |new_athlete| {
                    ftp_cell.set(new_athlete.ftp_watts);
                    *athlete_for_pages.borrow_mut() = new_athlete.clone();
                    engine_on_save.borrow_mut().athlete = new_athlete;
                },
                move |rate| {
                    engine_for_erg.borrow_mut().erg_ramp_rate = rate;
                },
            );
        });
        app.add_action(&prefs_action);

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
                        if connected {
                            player_for_loop
                                .borrow()
                                .add_connected_device(&address, &display_name);
                            toast_overlay_for_loop.add_toast(
                                adw::Toast::builder()
                                    .title(format!("Connected: {}", display_name))
                                    .timeout(4)
                                    .build(),
                            );
                        } else {
                            player_for_loop.borrow().remove_connected_device(&address);
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
