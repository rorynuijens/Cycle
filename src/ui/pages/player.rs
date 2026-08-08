use adw::prelude::*;
use gtk::glib;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

type StartNowCb = Rc<RefCell<Option<Box<dyn Fn()>>>>;
type ButtonCb = Rc<RefCell<Option<Box<dyn Fn()>>>>;

use crate::data::{
    athlete::AthleteProfile,
    session::{LiveReadings, ReadingsTracker, Session},
    workout::Workout,
};
use crate::training::engine::{EngineSnapshot, EngineState, WorkoutEngine};
use crate::ui::widgets::workout_graph::WorkoutGraph;
use crate::ui::widgets::zone_meter::ZoneMeter;

pub struct PlayerPage {
    root: gtk::Box,
    power_label: gtk::Label,
    hr_label: gtk::Label,
    cadence_label: gtk::Label,
    target_label: gtk::Label,
    elapsed_label: gtk::Label,
    remaining_label: gtk::Label,
    interval_label: gtk::Label,
    workout_progress: gtk::ProgressBar,
    segment_progress: gtk::ProgressBar,
    /// Caption above the interval countdown — shows "Interval N" while running.
    interval_caption: gtk::Label,
    /// Live power-zone ribbon under the hero power number.
    zone_meter: ZoneMeter,
    /// Shown during the pre-start countdown — hides once the engine starts.
    countdown_banner: adw::Banner,
    /// Raised mid-ride when a sensor stops reporting. A dropout used to show up
    /// only as a chip quietly leaving the row above, which is far too easy to
    /// miss while riding hard.
    dropout_banner: adw::Banner,
    /// Horizontal pill row listing connected BLE devices.
    devices_section: gtk::Box,
    devices_flow: gtk::Box,
    graph: WorkoutGraph,
    avg_power_total: gtk::Label,
    np_total: gtk::Label,
    if_total: gtk::Label,
    tss_total: gtk::Label,
    kj_total: gtk::Label,
    pause_btn: gtk::Button,
    #[allow(dead_code)]
    skip_btn: gtk::Button,
    end_btn: gtk::Button,
    /// Visible only while the engine is in Idle state; lets the user back out
    /// before the workout has actually started.
    cancel_btn: gtk::Button,
    /// Latest readings from the device manager, updated by the GLib event loop.
    pub last_readings: Rc<RefCell<ReadingsTracker>>,
    /// Consecutive seconds of power data received while the engine is Idle.
    pub power_countdown: Rc<Cell<u32>>,
    /// Callback wired by `start_timer` for the countdown banner's "Start now" button.
    start_now_cb: StartNowCb,
    /// Playback-control callbacks — set (and replaced) each time `start_timer` is called,
    /// so that buttons are wired exactly once in `new()` and never accumulate handlers.
    end_cb: ButtonCb,
    pause_cb: ButtonCb,
    skip_cb: ButtonCb,
    cancel_cb: ButtonCb,
    /// Shows the active workout name above the graph.
    workout_name_label: gtk::Label,
}

impl PlayerPage {
    pub fn new(workout: &Workout, athlete: &AthleteProfile) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();

        // ── Countdown banner (pre-start) ─────────────────────────────────────
        let countdown_banner = adw::Banner::builder()
            .title("Connect a power meter or trainer to begin")
            .button_label("Start now")
            .revealed(true)
            .build();

        let start_now_cb: StartNowCb = Rc::new(RefCell::new(None));
        let start_now_ref = Rc::clone(&start_now_cb);
        countdown_banner.connect_button_clicked(move |_| {
            if let Some(cb) = start_now_ref.borrow().as_ref() {
                cb();
            }
        });

        root.append(&countdown_banner);

        // ── Dropout banner (mid-ride) ────────────────────────────────────────
        let dropout_banner = adw::Banner::builder().revealed(false).build();
        root.append(&dropout_banner);

        // ── Cockpit layout ───────────────────────────────────────────────────
        // Everything the rider needs mid-effort is visible at once — no scroll.
        // The graph absorbs spare height so the page fills any window size.
        let clamp = adw::Clamp::builder()
            .maximum_size(900)
            .margin_top(18)
            .margin_bottom(24)
            .margin_start(24)
            .margin_end(24)
            .vexpand(true)
            .build();

        let inner = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .build();

        // ── Header row: workout name + connected device chips ────────────────
        let header_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .build();
        let workout_name_label = gtk::Label::builder()
            .label(&workout.name)
            .halign(gtk::Align::Start)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["title-3"])
            .build();
        header_row.append(&workout_name_label);

        let devices_section = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .visible(false)
            .halign(gtk::Align::End)
            .build();
        devices_section.append(
            &gtk::Label::builder()
                .label("Connected:")
                .css_classes(["caption", "dim-label"])
                .build(),
        );
        let devices_flow = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        devices_section.append(&devices_flow);
        header_row.append(&devices_section);
        inner.append(&header_row);

        // ── Hero row: target | live power | interval countdown ───────────────
        // Power is the page's reason to exist — it gets the `display` class
        // (CLAUDE.md §1.5) and the centre column; the machine-set target and
        // the interval countdown flank it.
        let hero_grid = gtk::Grid::builder()
            .column_spacing(12)
            .column_homogeneous(true)
            .build();

        let (target_box, target_label) =
            Self::metric_column("Target", "— W", &["title-1", "numeric", "accent"]);
        hero_grid.attach(&target_box, 0, 0, 1, 1);

        let (power_box, power_label) = Self::metric_column("Power", "— W", &["display", "numeric"]);
        hero_grid.attach(&power_box, 1, 0, 1, 1);

        let interval_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::End)
            .build();
        let interval_caption = gtk::Label::builder()
            .label("Interval")
            .css_classes(["caption", "dim-label"])
            .build();
        let interval_label = gtk::Label::builder()
            .label("—")
            .css_classes(["title-1", "numeric"])
            .build();
        let segment_progress = gtk::ProgressBar::builder().fraction(0.0).build();
        segment_progress.add_css_class("accent");
        interval_box.append(&interval_caption);
        interval_box.append(&interval_label);
        interval_box.append(&segment_progress);
        hero_grid.attach(&interval_box, 2, 0, 1, 1);
        inner.append(&hero_grid);

        // ── Zone ribbon: where the current effort sits across Z1–Z7 ─────────
        let zone_meter = ZoneMeter::new(athlete.ftp_watts);
        inner.append(zone_meter.widget());

        // ── Secondary metrics: HR · cadence · elapsed · remaining ────────────
        let secondary_grid = gtk::Grid::builder()
            .column_spacing(12)
            .column_homogeneous(true)
            .build();
        let (hr_box, hr_label) =
            Self::metric_column("Heart Rate", "— bpm", &["title-2", "numeric"]);
        let (cadence_box, cadence_label) =
            Self::metric_column("Cadence", "— rpm", &["title-2", "numeric"]);
        let (elapsed_box, elapsed_label) =
            Self::metric_column("Elapsed", "0:00", &["title-2", "numeric"]);
        let (remaining_box, remaining_label) = Self::metric_column(
            "Remaining",
            &WorkoutEngine::format_duration(workout.duration_secs),
            &["title-2", "numeric"],
        );
        secondary_grid.attach(&hr_box, 0, 0, 1, 1);
        secondary_grid.attach(&cadence_box, 1, 0, 1, 1);
        secondary_grid.attach(&elapsed_box, 2, 0, 1, 1);
        secondary_grid.attach(&remaining_box, 3, 0, 1, 1);
        inner.append(&secondary_grid);

        // ── Workout power profile graph (absorbs spare height) ───────────────
        let graph = WorkoutGraph::new(workout, athlete.ftp_watts);
        graph.widget().set_vexpand(true);
        inner.append(graph.widget());

        // ── Whole-workout progress bar ───────────────────────────────────────
        let workout_progress = gtk::ProgressBar::builder()
            .fraction(0.0)
            .show_text(false)
            .build();
        workout_progress.add_css_class("accent");
        inner.append(&workout_progress);

        // ── Session totals strip ─────────────────────────────────────────────
        // Live running totals in one glanceable line; the full breakdown
        // belongs to the post-ride summary page.
        let totals_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(18)
            .halign(gtk::Align::Center)
            .build();

        let make_total_pair = |name: &str, tooltip: Option<&str>| -> gtk::Label {
            let pair = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(6)
                .build();
            let name_lbl = gtk::Label::builder()
                .label(name)
                .css_classes(["caption", "dim-label"])
                .build();
            let value_lbl = gtk::Label::builder()
                .label("—")
                .css_classes(["caption", "numeric"])
                .build();
            if let Some(tip) = tooltip {
                pair.set_tooltip_text(Some(tip));
            }
            pair.append(&name_lbl);
            pair.append(&value_lbl);
            totals_row.append(&pair);
            value_lbl
        };

        let avg_power_total = make_total_pair("Avg", Some("Average power"));
        let np_total = make_total_pair("NP", Some("Normalised power"));
        let if_total = make_total_pair("IF", Some("Intensity factor"));
        let tss_total = make_total_pair("TSS", Some("Training stress score"));
        let kj_total = make_total_pair("Energy", Some("Total work in kilojoules"));
        inner.append(&totals_row);

        // ── Playback controls ────────────────────────────────────────────────
        // Cancel/Pause/Skip at leading edge; hexpand spacer pushes End Workout to
        // the trailing edge so the destructive action is visually separated per HIG.
        let controls = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .hexpand(true)
            .build();

        let cancel_btn = gtk::Button::builder()
            .label("Cancel")
            .css_classes(["flat", "pill"])
            .tooltip_text("Cancel and return to dashboard")
            .build();
        let pause_btn = gtk::Button::builder()
            .icon_name("media-playback-pause-symbolic")
            .tooltip_text("Pause workout")
            .css_classes(["circular", "flat"])
            .build();
        let skip_btn = gtk::Button::builder()
            .icon_name("media-skip-forward-symbolic")
            .tooltip_text("Skip to next interval")
            .css_classes(["circular", "flat"])
            .build();
        let end_btn = gtk::Button::builder()
            .label("End Workout")
            .css_classes(["destructive-action", "pill"])
            .tooltip_text("End the current workout")
            .build();

        let controls_spacer = gtk::Box::builder().hexpand(true).build();

        controls.append(&cancel_btn);
        controls.append(&pause_btn);
        controls.append(&skip_btn);
        controls.append(&controls_spacer);
        controls.append(&end_btn);
        inner.append(&controls);

        clamp.set_child(Some(&inner));
        root.append(&clamp);

        // ── Wire buttons once through replaceable callbacks ───────────────────
        // Each start_timer call replaces the callbacks; the buttons are never
        // connected more than once, preventing handler accumulation.
        let end_cb: ButtonCb = Rc::new(RefCell::new(None));
        let pause_cb: ButtonCb = Rc::new(RefCell::new(None));
        let skip_cb: ButtonCb = Rc::new(RefCell::new(None));
        let cancel_cb: ButtonCb = Rc::new(RefCell::new(None));

        {
            let cb = Rc::clone(&end_cb);
            end_btn.connect_clicked(move |_| {
                if let Some(f) = cb.borrow().as_ref() {
                    f();
                }
            });
        }
        {
            let cb = Rc::clone(&pause_cb);
            pause_btn.connect_clicked(move |_| {
                if let Some(f) = cb.borrow().as_ref() {
                    f();
                }
            });
        }
        {
            let cb = Rc::clone(&skip_cb);
            skip_btn.connect_clicked(move |_| {
                if let Some(f) = cb.borrow().as_ref() {
                    f();
                }
            });
        }
        {
            let cb = Rc::clone(&cancel_cb);
            cancel_btn.connect_clicked(move |_| {
                if let Some(f) = cb.borrow().as_ref() {
                    f();
                }
            });
        }

        Self {
            root,
            power_label,
            hr_label,
            cadence_label,
            target_label,
            elapsed_label,
            remaining_label,
            interval_label,
            workout_progress,
            segment_progress,
            interval_caption,
            zone_meter,
            countdown_banner,
            dropout_banner,
            devices_section,
            devices_flow,
            graph,
            avg_power_total,
            np_total,
            if_total,
            tss_total,
            kj_total,
            pause_btn,
            skip_btn,
            end_btn,
            last_readings: Rc::new(RefCell::new(ReadingsTracker::default())),
            power_countdown: Rc::new(Cell::new(0)),
            start_now_cb,
            cancel_btn,
            end_cb,
            pause_cb,
            skip_cb,
            cancel_cb,
            workout_name_label,
        }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// Merge incoming readings into the stored state.
    pub fn set_readings(&self, readings: LiveReadings) {
        self.last_readings
            .borrow_mut()
            .merge(readings, std::time::Instant::now());
    }

    /// Raise or clear the dropout banner for the sensors that have gone quiet.
    ///
    /// Driven by sensor staleness rather than by BLE connection events, so it
    /// also catches a device that is still nominally connected but has stopped
    /// sending — which looks identical to the rider.
    pub fn set_stale_sensors(&self, stale: &[&'static str]) {
        match crate::ui::dropout_banner_text(stale) {
            Some(title) => {
                self.dropout_banner.set_title(&title);
                self.dropout_banner.set_revealed(true);
            }
            None => self.dropout_banner.set_revealed(false),
        }
    }

    /// Add a connected-device chip to the status row.
    pub fn add_connected_device(&self, address: &str, display_name: &str) {
        // Guard against duplicates
        let mut child = self.devices_flow.first_child();
        while let Some(c) = child {
            if c.widget_name() == address {
                return;
            }
            child = c.next_sibling();
        }

        let chip = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(8)
            .margin_end(8)
            .build();
        chip.add_css_class("card");
        chip.set_widget_name(address);
        chip.append(
            &gtk::Image::builder()
                .icon_name("bluetooth-symbolic")
                .css_classes(["dim-label"])
                .build(),
        );
        chip.append(
            &gtk::Label::builder()
                .label(display_name)
                .css_classes(["caption"])
                .build(),
        );

        self.devices_flow.append(&chip);
        self.devices_section.set_visible(true);
    }

    /// Remove a connected-device chip by address.
    pub fn remove_connected_device(&self, address: &str) {
        let mut child = self.devices_flow.first_child();
        while let Some(c) = child {
            let next = c.next_sibling();
            if c.widget_name() == address {
                self.devices_flow.remove(&c);
                break;
            }
            child = next;
        }
        if self.devices_flow.first_child().is_none() {
            self.devices_section.set_visible(false);
        }
    }

    /// Reset all UI for a new workout without rebuilding the widget tree.
    pub fn reset_workout(&self, workout: &Workout, ftp_watts: u32) {
        self.graph.set_ftp(ftp_watts);
        self.zone_meter.set_ftp(ftp_watts);
        self.workout_name_label.set_label(&workout.name);
        self.cancel_btn.set_visible(true);
        self.countdown_banner
            .set_title("Connect a power meter or trainer to begin");
        self.countdown_banner.set_revealed(true);
        self.dropout_banner.set_revealed(false);
        self.power_countdown.set(0);
        self.graph.set_workout(workout);
        self.zone_meter.set_power(None);
        self.elapsed_label.set_label("0:00");
        self.remaining_label
            .set_label(&WorkoutEngine::format_duration(workout.duration_secs));
        self.workout_progress.set_fraction(0.0);
        self.segment_progress.set_fraction(0.0);
        self.power_label.set_label("— W");
        self.hr_label.set_label("— bpm");
        self.cadence_label.set_label("— rpm");
        self.target_label.set_label("— W");
        self.interval_label.set_label("—");
        self.interval_caption.set_label("Interval");
        self.avg_power_total.set_label("—");
        self.np_total.set_label("—");
        self.if_total.set_label("—");
        self.tss_total.set_label("—");
        self.kj_total.set_label("—");
    }

    /// Invoke the pause/resume toggle programmatically.
    ///
    /// The pause/resume button toggles based on the engine state, so the caller is
    /// responsible for only triggering this when a state change is wanted — e.g. the
    /// header "Resume Workout" button calls it only when the engine is paused, so it
    /// never accidentally pauses a running workout.
    pub fn trigger_pause_toggle(&self) {
        if let Some(cb) = self.pause_cb.borrow().as_ref() {
            cb();
        }
    }

    /// Start the 1 Hz GLib timer, wire the playback controls, and call `on_complete`
    /// (exactly once) when the workout finishes.
    ///
    /// The engine starts automatically after 10 consecutive seconds of power data.
    /// The countdown banner's "Start now" button bypasses the wait.
    pub fn start_timer(
        page: Rc<RefCell<Self>>,
        engine: Rc<RefCell<WorkoutEngine>>,
        on_complete: impl Fn(Session) + 'static,
        on_cancel: impl Fn() + 'static,
        timer_alive: Rc<Cell<bool>>,
    ) {
        timer_alive.set(true);
        let on_complete: Rc<dyn Fn(Session)> = Rc::new(on_complete);
        let completed = Rc::new(Cell::new(false));

        // ── Wire Cancel button (Idle-state escape hatch) ─────────────────────
        {
            let engine_cancel = Rc::clone(&engine);
            let timer_alive_cancel = Rc::clone(&timer_alive);
            *page.borrow().cancel_cb.borrow_mut() = Some(Box::new(move || {
                if engine_cancel.borrow().state == EngineState::Idle {
                    timer_alive_cancel.set(false);
                    on_cancel();
                }
            }));
        }

        // ── Wire "Start now" button ──────────────────────────────────────────
        {
            let engine_start_now = Rc::clone(&engine);
            let power_countdown_now = Rc::clone(&page.borrow().power_countdown);
            let countdown_banner_now = page.borrow().countdown_banner.clone();
            *page.borrow().start_now_cb.borrow_mut() = Some(Box::new(move || {
                engine_start_now.borrow_mut().start();
                power_countdown_now.set(0);
                countdown_banner_now.set_revealed(false);
            }));
        }

        // ── Pause / resume ───────────────────────────────────────────────────
        {
            let pause_btn = page.borrow().pause_btn.clone();
            let engine_pause = Rc::clone(&engine);
            *page.borrow().pause_cb.borrow_mut() = Some(Box::new(move || {
                let mut eng = engine_pause.borrow_mut();
                match eng.state {
                    EngineState::Running => {
                        eng.pause();
                        pause_btn.set_icon_name("media-playback-start-symbolic");
                        pause_btn.set_tooltip_text(Some("Resume workout"));
                    }
                    EngineState::Paused => {
                        eng.resume();
                        pause_btn.set_icon_name("media-playback-pause-symbolic");
                        pause_btn.set_tooltip_text(Some("Pause workout"));
                    }
                    _ => {}
                }
            }));
        }

        // ── Skip to next interval ────────────────────────────────────────────
        {
            let engine_skip = Rc::clone(&engine);
            *page.borrow().skip_cb.borrow_mut() = Some(Box::new(move || {
                engine_skip.borrow_mut().skip_to_next_segment();
            }));
        }

        // ── End Workout ──────────────────────────────────────────────────────
        {
            let end_btn = page.borrow().end_btn.clone();
            let engine_end = Rc::clone(&engine);
            let on_complete_end = Rc::clone(&on_complete);
            let completed_end = Rc::clone(&completed);
            *page.borrow().end_cb.borrow_mut() = Some(Box::new(move || {
                let dialog = adw::AlertDialog::builder()
                    .heading("End Workout?")
                    .body("Your progress so far will be saved.")
                    .build();
                dialog.add_response("cancel", "_Cancel");
                dialog.add_response("end", "_End Workout");
                dialog.set_response_appearance("end", adw::ResponseAppearance::Destructive);
                dialog.set_default_response(Some("cancel"));
                dialog.set_close_response("cancel");

                let engine_resp = Rc::clone(&engine_end);
                let on_complete_resp = Rc::clone(&on_complete_end);
                let completed_resp = Rc::clone(&completed_end);
                dialog.connect_response(None, move |_, response| {
                    if response == "end" && !completed_resp.get() {
                        completed_resp.set(true);
                        let session = {
                            let mut eng = engine_resp.borrow_mut();
                            eng.stop();
                            eng.session.clone()
                        };
                        on_complete_resp(session);
                    }
                });
                dialog.present(Some(&end_btn));
            }));
        }

        // ── 1 Hz tick loop ───────────────────────────────────────────────────
        let last_readings_rc = Rc::clone(&page.borrow().last_readings);
        // Sensors already reported as dropped, so each is logged once per ride.
        let stale_warned: Rc<RefCell<std::collections::HashSet<&'static str>>> =
            Rc::new(RefCell::new(std::collections::HashSet::new()));
        let power_countdown = Rc::clone(&page.borrow().power_countdown);
        let page_clone = Rc::clone(&page);
        let engine_clone = Rc::clone(&engine);
        let on_complete_timer = Rc::clone(&on_complete);
        let completed_timer = Rc::clone(&completed);
        let timer_alive_in_timer = Rc::clone(&timer_alive);

        glib::timeout_add_local(Duration::from_secs(1), move || {
            if !timer_alive_in_timer.get() {
                return glib::ControlFlow::Break;
            }
            // Only sensors still transmitting contribute — a strap that has gone
            // quiet must not have its last value recorded for the rest of the ride.
            let now = std::time::Instant::now();
            let readings = last_readings_rc.borrow().current(now);
            let stale = last_readings_rc.borrow().stale_sensors(now);
            for sensor in &stale {
                if stale_warned.borrow_mut().insert(*sensor) {
                    tracing::warn!("{sensor} sensor stopped reporting — dropping it from the ride");
                }
            }
            page_clone.borrow().set_stale_sensors(&stale);

            let (snapshot, session, ftp) = {
                let mut eng = engine_clone.borrow_mut();

                // Auto-start: count consecutive seconds of power data.
                // After 10 seconds, start the engine automatically.
                if eng.state == EngineState::Idle {
                    if readings.power_watts.is_some() {
                        let n = power_countdown.get() + 1;
                        power_countdown.set(n);
                        if n >= 10 {
                            eng.start();
                            power_countdown.set(0);
                        }
                    } else {
                        power_countdown.set(0);
                    }
                }

                let snapshot = eng.tick(readings);
                let session = eng.session.clone();
                let ftp = eng.athlete.borrow().ftp_watts;
                (snapshot, session, ftp)
            };

            // Update countdown banner title while still waiting.
            if snapshot.state == EngineState::Idle {
                let n = power_countdown.get();
                let title = if n == 0 {
                    "Connect a power meter or trainer to begin".to_string()
                } else {
                    format!("Starting in {} s…", 10u32.saturating_sub(n))
                };
                page_clone.borrow().countdown_banner.set_title(&title);
            }

            let p = page_clone.borrow();
            p.update_from_snapshot(&snapshot);
            p.update_session_totals(&session, ftp);
            p.graph.set_playhead(snapshot.elapsed_secs);
            p.graph.push_power(snapshot.readings.power_watts);
            drop(p);

            if snapshot.state == EngineState::Completed {
                if !completed_timer.get() {
                    completed_timer.set(true);
                    on_complete_timer(session);
                }
                return glib::ControlFlow::Break;
            }

            glib::ControlFlow::Continue
        });
    }

    pub fn update_from_snapshot(&self, snap: &EngineSnapshot) {
        self.power_label.set_label(&format!(
            "{} W",
            snap.readings
                .power_watts
                .map(|w| w.to_string())
                .unwrap_or_else(|| "—".into())
        ));
        self.hr_label.set_label(&format!(
            "{} bpm",
            snap.readings
                .heart_rate_bpm
                .map(|h| h.to_string())
                .unwrap_or_else(|| "—".into())
        ));
        self.cadence_label.set_label(&format!(
            "{} rpm",
            snap.readings
                .cadence_rpm
                .map(|c| c.to_string())
                .unwrap_or_else(|| "—".into())
        ));
        self.target_label
            .set_label(&format!("{} W", snap.target_power_watts));
        self.zone_meter.set_power(snap.readings.power_watts);

        self.elapsed_label
            .set_label(&WorkoutEngine::format_duration(snap.elapsed_secs));
        self.remaining_label
            .set_label(&WorkoutEngine::format_duration(snap.remaining_secs));
        self.interval_label
            .set_label(&WorkoutEngine::format_duration(snap.segment_remaining_secs));

        let total = snap.elapsed_secs + snap.remaining_secs;
        if total > 0 {
            self.workout_progress
                .set_fraction(snap.elapsed_secs as f64 / total as f64);
        }
        let seg_total = snap.segment_elapsed_secs + snap.segment_remaining_secs;
        if seg_total > 0 {
            self.segment_progress
                .set_fraction(snap.segment_elapsed_secs as f64 / seg_total as f64);
        }

        // Interval caption: numbered while the workout is active.
        let is_active = matches!(snap.state, EngineState::Running | EngineState::Paused);
        if is_active {
            self.interval_caption
                .set_label(&format!("Interval {}", snap.segment_index + 1));
        } else {
            self.interval_caption.set_label("Interval");
        }

        // Countdown banner and cancel button: visible only while engine is still Idle.
        let is_idle = snap.state == EngineState::Idle;
        self.countdown_banner.set_revealed(is_idle);
        self.cancel_btn.set_visible(is_idle);
    }

    /// Update the live session totals strip from the current in-progress session.
    pub fn update_session_totals(&self, session: &Session, ftp: u32) {
        self.zone_meter.set_ftp(ftp);
        // The page outlives any one ride, so the graph's FTP scale has to track
        // the profile rather than keep the value it was built with.
        self.graph.set_ftp(ftp);
        match session.average_power() {
            Some(p) => self.avg_power_total.set_label(&format!("{} W", p as u32)),
            None => self.avg_power_total.set_label("—"),
        }
        let np = session.normalised_power();
        match np {
            Some(p) => self.np_total.set_label(&format!("{} W", p as u32)),
            None => self.np_total.set_label("—"),
        }
        match np {
            Some(p) => self.if_total.set_label(&format!("{:.2}", p / ftp as f32)),
            None => self.if_total.set_label("—"),
        }
        match session.tss(ftp) {
            Some(t) => self.tss_total.set_label(&format!("{}", t as u32)),
            None => self.tss_total.set_label("—"),
        }
        let kj = session.kilojoules();
        self.kj_total.set_label(&format!("{:.0} kJ", kj));
    }

    /// A centred caption-over-value column for the cockpit metric rows.
    fn metric_column(title: &str, initial: &str, value_css: &[&str]) -> (gtk::Box, gtk::Label) {
        let vbox = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::End)
            .build();

        vbox.append(
            &gtk::Label::builder()
                .label(title)
                .css_classes(["caption", "dim-label"])
                .build(),
        );

        let value_label = gtk::Label::builder()
            .label(initial)
            .css_classes(value_css.to_vec())
            .build();

        vbox.append(&value_label);
        (vbox, value_label)
    }
}
