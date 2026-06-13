use adw::prelude::*;
use gtk::glib;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

type StartNowCb = Rc<RefCell<Option<Box<dyn Fn()>>>>;
type ButtonCb = Rc<RefCell<Option<Box<dyn Fn()>>>>;

use crate::data::{
    athlete::AthleteProfile,
    session::{LiveReadings, Session},
    workout::Workout,
};
use crate::training::engine::{EngineSnapshot, EngineState, WorkoutEngine};
use crate::ui::widgets::workout_graph::WorkoutGraph;

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
    /// Shown while the workout is running — hides during countdown.
    /// Compact interval info row shown while the workout is active (above the scroll).
    interval_revealer: gtk::Revealer,
    interval_info_label: gtk::Label,
    /// Shown during the pre-start countdown — hides once the engine starts.
    countdown_banner: adw::Banner,
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
    pub last_readings: Rc<RefCell<LiveReadings>>,
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

        // ── Interval info row (shown while workout is running) ────────────────
        // adw::Banner is for dismissible alerts; persistent live state uses a plain row.
        let interval_info_label = gtk::Label::builder()
            .label("")
            .css_classes(["caption", "numeric"])
            .hexpand(true)
            .halign(gtk::Align::Start)
            .build();
        let interval_info_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(18)
            .margin_end(18)
            .build();
        interval_info_row.append(&interval_info_label);
        let rev_inner = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        rev_inner.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        rev_inner.append(&interval_info_row);
        let interval_revealer = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .transition_duration(150)
            .reveal_child(false)
            .child(&rev_inner)
            .build();
        root.append(&interval_revealer);

        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();

        let clamp = adw::Clamp::builder()
            .maximum_size(900)
            .margin_top(20)
            .margin_bottom(24)
            .margin_start(24)
            .margin_end(24)
            .build();

        let inner = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .build();

        // ── Connected device chips ────────────────────────────────────────────
        let devices_section = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .visible(false)
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
        inner.append(&devices_section);

        // ── Workout name ─────────────────────────────────────────────────────
        let workout_name_label = gtk::Label::builder()
            .label(&workout.name)
            .halign(gtk::Align::Start)
            .css_classes(["title-3"])
            .build();
        inner.append(&workout_name_label);

        // ── Workout power profile graph ──────────────────────────────────────
        let graph = WorkoutGraph::new(workout, athlete);
        inner.append(graph.widget());

        // ── Whole-workout progress bar ───────────────────────────────────────
        let workout_progress = gtk::ProgressBar::builder()
            .fraction(0.0)
            .show_text(false)
            .build();
        workout_progress.add_css_class("accent");
        inner.append(&workout_progress);

        let time_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .build();
        let elapsed_label = gtk::Label::builder()
            .label("0:00")
            .css_classes(["dim-label", "caption", "numeric"])
            .build();
        let remaining_label = gtk::Label::builder()
            .label(WorkoutEngine::format_duration(workout.duration_secs))
            .css_classes(["dim-label", "caption", "numeric"])
            .halign(gtk::Align::End)
            .hexpand(true)
            .build();
        time_row.append(&elapsed_label);
        time_row.append(&remaining_label);
        inner.append(&time_row);

        // ── 2×2 live metric grid ─────────────────────────────────────────────
        let metrics_grid = gtk::Grid::builder()
            .row_spacing(12)
            .column_spacing(12)
            .column_homogeneous(true)
            .build();

        let (power_frame, power_label) = Self::metric_card("Power", "— W", &["accent"]);
        let (hr_frame, hr_label) = Self::metric_card("Heart Rate", "— bpm", &[]);
        let (cadence_frame, cadence_label) = Self::metric_card("Cadence", "— rpm", &[]);
        let (target_frame, target_label) = Self::metric_card("Target", "— W", &["accent"]);

        metrics_grid.attach(&power_frame, 0, 0, 1, 1);
        metrics_grid.attach(&hr_frame, 1, 0, 1, 1);
        metrics_grid.attach(&cadence_frame, 0, 1, 1, 1);
        metrics_grid.attach(&target_frame, 1, 1, 1, 1);
        inner.append(&metrics_grid);

        // ── Interval countdown ───────────────────────────────────────────────
        let interval_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .build();

        interval_box.append(
            &gtk::Label::builder()
                .label("Current Interval")
                .css_classes(["title-4"])
                .halign(gtk::Align::Start)
                .build(),
        );

        let interval_label = gtk::Label::builder()
            .label("—")
            .css_classes(["title-2", "accent", "numeric"])
            .halign(gtk::Align::Start)
            .build();

        let segment_progress = gtk::ProgressBar::builder().fraction(0.0).build();
        segment_progress.add_css_class("accent");

        interval_box.append(&interval_label);
        interval_box.append(&segment_progress);
        inner.append(&interval_box);

        // ── Session totals ───────────────────────────────────────────────────
        let totals_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .build();

        totals_box.append(
            &gtk::Label::builder()
                .label("Session Totals")
                .halign(gtk::Align::Start)
                .css_classes(["heading"])
                .build(),
        );

        let totals_list = gtk::ListBox::builder()
            .css_classes(["boxed-list"])
            .selection_mode(gtk::SelectionMode::None)
            .build();

        let make_total_row = |title: &str| -> (adw::ActionRow, gtk::Label) {
            let row = adw::ActionRow::builder().title(title).build();
            let lbl = gtk::Label::builder()
                .label("—")
                .css_classes(["dim-label", "numeric"])
                .valign(gtk::Align::Center)
                .build();
            row.add_suffix(&lbl);
            (row, lbl)
        };

        let (r, avg_power_total) = make_total_row("Avg Power");
        totals_list.append(&r);
        let (r, np_total) = make_total_row("Normalised Power");
        totals_list.append(&r);
        let (r, if_total) = make_total_row("Intensity Factor");
        totals_list.append(&r);
        let (r, tss_total) = make_total_row("TSS");
        totals_list.append(&r);
        let (r, kj_total) = make_total_row("Kilojoules");
        totals_list.append(&r);

        totals_box.append(&totals_list);
        inner.append(&totals_box);

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
        scroll.set_child(Some(&clamp));
        root.append(&scroll);

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
            interval_revealer,
            interval_info_label,
            countdown_banner,
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
            last_readings: Rc::new(RefCell::new(LiveReadings::default())),
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
        let mut stored = self.last_readings.borrow_mut();
        if readings.power_watts.is_some() {
            stored.power_watts = readings.power_watts;
        }
        if readings.heart_rate_bpm.is_some() {
            stored.heart_rate_bpm = readings.heart_rate_bpm;
        }
        if readings.cadence_rpm.is_some() {
            stored.cadence_rpm = readings.cadence_rpm;
        }
        if readings.speed_kmh.is_some() {
            stored.speed_kmh = readings.speed_kmh;
        }
        if readings.resistance_target_watts.is_some() {
            stored.resistance_target_watts = readings.resistance_target_watts;
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
    pub fn reset_workout(&self, workout: &Workout) {
        self.workout_name_label.set_label(&workout.name);
        self.cancel_btn.set_visible(true);
        self.countdown_banner
            .set_title("Connect a power meter or trainer to begin");
        self.countdown_banner.set_revealed(true);
        self.power_countdown.set(0);
        self.interval_revealer.set_reveal_child(false);
        self.graph.set_workout(workout);
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
        self.avg_power_total.set_label("—");
        self.np_total.set_label("—");
        self.if_total.set_label("—");
        self.tss_total.set_label("—");
        self.kj_total.set_label("—");
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
            let readings = last_readings_rc.borrow().clone();

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
                let ftp = eng.athlete.ftp_watts;
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

        self.elapsed_label
            .set_label(&WorkoutEngine::format_duration(snap.elapsed_secs));
        self.remaining_label
            .set_label(&WorkoutEngine::format_duration(snap.remaining_secs));
        self.interval_label.set_label(&format!(
            "{} remaining",
            WorkoutEngine::format_duration(snap.segment_remaining_secs)
        ));

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

        // Interval info row: visible while the workout is active.
        let is_active = matches!(snap.state, EngineState::Running | EngineState::Paused);
        self.interval_revealer.set_reveal_child(is_active);
        if is_active {
            self.interval_info_label.set_label(&format!(
                "Interval {} — {} remaining",
                snap.segment_index + 1,
                WorkoutEngine::format_duration(snap.segment_remaining_secs),
            ));
        }

        // Countdown banner and cancel button: visible only while engine is still Idle.
        let is_idle = snap.state == EngineState::Idle;
        self.countdown_banner.set_revealed(is_idle);
        self.cancel_btn.set_visible(is_idle);
    }

    /// Update the live session totals panel from the current in-progress session.
    pub fn update_session_totals(&self, session: &Session, ftp: u32) {
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

    fn metric_card(title: &str, initial: &str, value_css: &[&str]) -> (gtk::Box, gtk::Label) {
        let card = gtk::Box::builder()
            .css_classes(["card"])
            .hexpand(true)
            .vexpand(true)
            .build();

        let vbox = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_top(18)
            .margin_bottom(18)
            .margin_start(18)
            .margin_end(18)
            .valign(gtk::Align::Center)
            .build();

        vbox.append(
            &gtk::Label::builder()
                .label(title)
                .css_classes(["caption", "dim-label"])
                .halign(gtk::Align::Start)
                .build(),
        );

        let mut classes = vec!["title-1", "numeric"];
        classes.extend_from_slice(value_css);

        let value_label = gtk::Label::builder()
            .label(initial)
            .css_classes(classes)
            .halign(gtk::Align::Start)
            .build();

        vbox.append(&value_label);
        card.append(&vbox);

        (card, value_label)
    }
}
