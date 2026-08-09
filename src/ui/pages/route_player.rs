use adw::prelude::*;
use gtk::glib;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use crate::data::athlete::AthleteProfile;
use crate::data::route::Route;
use crate::data::session::{DataPoint, LiveReadings, ReadingsTracker, Session};
use crate::devices::manager::DeviceCommand;
use crate::training::engine::{INTENSITY_STEP_PCT, MAX_INTENSITY_PCT, MIN_INTENSITY_PCT};
use crate::training::route_engine::RouteEngine;
use crate::ui::widgets::route_map::RouteMap;
use crate::ui::widgets::zone_color::gradient_rgb;
use crate::ui::widgets::zone_meter::ZoneMeter;

type ButtonCb = Rc<RefCell<Option<Box<dyn Fn()>>>>;

/// Consecutive seconds of power data required before a route ride starts,
/// matching the workout player's pre-start gate.
const PRE_START_SECS: u32 = 10;

/// Stamp the moment the ride actually begins.
///
/// The session is created when the route page opens, which can be well before the
/// rider starts pedalling. Recording the real start keeps the ride's duration (and
/// its TSS) free of the waiting time, and lines the session up with what a head
/// unit or Intervals.icu records for the same ride.
fn stamp_start(session: &Rc<RefCell<Session>>) {
    session.borrow_mut().started_at = chrono::Utc::now();
}

/// Sample a route into `(distance km, elevation m, gradient %)` for the profile
/// chart, at about one sample per 25 m so the gradient colouring is smooth
/// without redrawing thousands of segments each tick.
fn profile_samples(route: &Route) -> Vec<(f32, f32, f32)> {
    const SAMPLE_SPACING_M: f32 = 25.0;
    let total = route.total_distance_m;
    if total <= 0.0 || route.points.len() < 2 {
        return Vec::new();
    }
    let steps = (total / SAMPLE_SPACING_M).ceil().max(1.0) as usize;
    (0..=steps)
        .map(|i| {
            let d = (i as f32 * SAMPLE_SPACING_M).min(total);
            (
                d / 1000.0,
                route.elevation_at(d),
                route.gradient_at(d) * 100.0,
            )
        })
        .collect()
}

pub struct RoutePlayerPage {
    root: gtk::Box,
    /// Shown during the pre-start countdown — hides once the ride begins.
    countdown_banner: adw::Banner,
    /// Raised mid-ride when a sensor stops reporting. A dropout used to show up
    /// only as a chip quietly leaving the row above, which is far too easy to
    /// miss while riding hard.
    dropout_banner: adw::Banner,
    /// Consecutive seconds of power data seen while waiting to start.
    power_countdown: Rc<Cell<u32>>,
    /// Callback wired by `start_timer` for the countdown banner's "Start now" button.
    start_now_cb: ButtonCb,
    route_name_label: gtk::Label,
    mode_label: gtk::Label,
    /// Wrapper around the connected-device chips — hidden until one connects.
    devices_section: gtk::Box,
    devices_flow: gtk::Box,
    power_label: gtk::Label,
    hr_label: gtk::Label,
    cadence_label: gtk::Label,
    climb_label: gtk::Label,
    gradient_label: gtk::Label,
    speed_label: gtk::Label,
    elapsed_label: gtk::Label,
    dist_remaining_label: gtk::Label,
    progress_bar: gtk::ProgressBar,
    /// Effort ribbon across Z1–Z7, as on the workout screen.
    zone_meter: ZoneMeter,
    /// Slippy map following the rider along the route.
    route_map: RouteMap,
    elevation_chart: gtk::DrawingArea,
    pause_btn: gtk::Button,
    end_btn: gtk::Button,
    /// The intensity dial for this ride, in percent of the road as it is.
    ///
    /// Per-ride and deliberately not written back to Preferences: the Trainer
    /// Difficulty setting stays the baseline this multiplies, so dialling a climb
    /// down mid-ride cannot silently follow the rider into every later ride.
    /// Read live by the ride loop, so it applies from the next tick.
    intensity_pct: Rc<Cell<u32>>,
    /// Reads the dial's position, e.g. "90%".
    intensity_label: gtk::Label,
    pub last_readings: Rc<RefCell<ReadingsTracker>>,
    end_cb: ButtonCb,
    pause_cb: ButtonCb,
    /// Current distance_m for the playhead marker on the chart.
    playhead_dist: Rc<Cell<f32>>,
    /// Profile samples as (distance km, elevation m, gradient %) — rebuilt by
    /// reset_route() for each new ride. The gradient colours the trace.
    ele_pts: Rc<RefCell<Vec<(f32, f32, f32)>>>,
    /// The ride currently being recorded, published so the checkpoint timer in
    /// `window.rs` can persist it. `None` outside a route ride.
    live_session: RefCell<Option<Rc<RefCell<Session>>>>,
}

impl RoutePlayerPage {
    pub fn new(route: &Route, ftp_watts: u32) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();

        // ── Countdown banner (pre-start) ─────────────────────────────────────
        // Same gate as the workout player: the ride waits for the rider to be
        // pedalling before the clock and the route start moving.
        let countdown_banner = adw::Banner::builder()
            .title("Connect a power meter or trainer to begin")
            .button_label("Start now")
            .revealed(true)
            .build();

        let start_now_cb: ButtonCb = Rc::new(RefCell::new(None));
        {
            let cb = Rc::clone(&start_now_cb);
            countdown_banner.connect_button_clicked(move |_| {
                if let Some(f) = cb.borrow().as_ref() {
                    f();
                }
            });
        }
        root.append(&countdown_banner);

        let dropout_banner = adw::Banner::builder().revealed(false).build();
        root.append(&dropout_banner);

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

        // ── Route name + ride mode ───────────────────────────────────────────
        let name_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .build();
        let route_name_label = gtk::Label::builder()
            .label(&route.name)
            .halign(gtk::Align::Start)
            .css_classes(["title-3"])
            .build();
        // Which mode drives the trainer: SIM (resistance follows the road) or
        // ERG fallback (fixed-speed power targets). Set by the ride loop.
        let mode_label = gtk::Label::builder()
            .css_classes(["caption", "dim-label"])
            .halign(gtk::Align::End)
            .hexpand(true)
            .build();
        name_row.append(&route_name_label);
        name_row.append(&mode_label);
        inner.append(&name_row);

        // ── Connected devices ────────────────────────────────────────────────
        // Same chip strip as the workout player, so the rider can see at a glance
        // which sensors are feeding the ride.
        let devices_section = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .visible(false)
            .halign(gtk::Align::Start)
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

        // ── Gradient profile ──────────────────────────────────────────────────
        let ele_pts_rc: Rc<RefCell<Vec<(f32, f32, f32)>>> =
            Rc::new(RefCell::new(profile_samples(route)));
        let playhead_dist = Rc::new(Cell::new(0.0f32));
        let total_dist_km = route.total_distance_m / 1000.0;

        let elevation_chart = gtk::DrawingArea::builder()
            .content_height(90)
            .hexpand(true)
            .accessible_role(gtk::AccessibleRole::Img)
            .build();
        elevation_chart.update_property(&[gtk::accessible::Property::Label(
            "Route gradient profile, coloured by steepness, with current position marker",
        )]);
        {
            let pts = Rc::clone(&ele_pts_rc);
            let ph = Rc::clone(&playhead_dist);
            elevation_chart.set_draw_func(move |area, cr, width, height| {
                let fg = area.color(); // theme foreground — works in light and dark
                let pts = pts.borrow();
                if pts.len() < 2 {
                    return;
                }
                let w = width as f64;
                let h = height as f64;
                let x_max = pts.last().map(|p| p.0).unwrap_or(1.0) as f64;
                let y_min = pts.iter().map(|p| p.1).fold(f32::INFINITY, f32::min) as f64;
                let y_max = pts.iter().map(|p| p.1).fold(f32::NEG_INFINITY, f32::max) as f64;
                let y_span = (y_max - y_min).max(1.0);
                let pad_t = 4.0;
                let usable = h - pad_t - 2.0;

                let to_x = |x: f32| x as f64 / x_max * w;
                let to_y = |y: f32| pad_t + (1.0 - (y as f64 - y_min) / y_span) * usable;

                // Fill, one quad per sample, coloured by that stretch's gradient
                // so climbs read at a glance in the same palette as power zones.
                for pair in pts.windows(2) {
                    let (p0, p1) = (pair[0], pair[1]);
                    let (r, g, b) = gradient_rgb(p1.2);
                    cr.set_source_rgba(r, g, b, 0.55);
                    cr.move_to(to_x(p0.0), h);
                    cr.line_to(to_x(p0.0), to_y(p0.1));
                    cr.line_to(to_x(p1.0), to_y(p1.1));
                    cr.line_to(to_x(p1.0), h);
                    cr.close_path();
                    cr.fill().ok();
                }

                // Ridge line, in the theme foreground so it reads on any fill.
                let (x0, y0) = (to_x(pts[0].0), to_y(pts[0].1));
                cr.set_source_rgba(fg.red() as f64, fg.green() as f64, fg.blue() as f64, 0.7);
                cr.set_line_width(1.5);
                cr.move_to(x0, y0);
                for p in &pts[1..] {
                    cr.line_to(to_x(p.0), to_y(p.1));
                }
                cr.stroke().ok();

                // Playhead
                let ph_km = ph.get() / 1000.0;
                let px = (ph_km as f64 / x_max * w).clamp(0.0, w);
                cr.set_source_rgba(0.47, 0.68, 0.93, 1.0);
                cr.set_line_width(2.0);
                cr.move_to(px, 0.0);
                cr.line_to(px, h);
                cr.stroke().ok();
            });
        }

        // ── Overall progress bar ──────────────────────────────────────────────
        let progress_bar = gtk::ProgressBar::builder()
            .fraction(0.0)
            .show_text(false)
            .build();
        progress_bar.add_css_class("accent");

        let time_dist_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .build();
        let elapsed_label = gtk::Label::builder()
            .label("0:00")
            .css_classes(["dim-label", "caption", "numeric"])
            .build();
        let dist_remaining_label = gtk::Label::builder()
            .label(format!("{:.1} km remaining", total_dist_km))
            .css_classes(["dim-label", "caption", "numeric"])
            .halign(gtk::Align::End)
            .hexpand(true)
            .build();
        time_dist_row.append(&elapsed_label);
        time_dist_row.append(&dist_remaining_label);

        // ── Hero row: gradient | live power | speed ──────────────────────────
        // Mirrors the workout screen: power is the page's reason to exist and
        // takes the centre with the `display` class (CLAUDE.md §1.5), flanked by
        // what the road is doing and what it is producing.
        let hero_grid = gtk::Grid::builder()
            .column_spacing(12)
            .column_homogeneous(true)
            .build();
        let (gradient_box, gradient_label) =
            Self::metric_column("Gradient", "— %", &["title-1", "numeric"]);
        let (power_box, power_label) = Self::metric_column("Power", "— W", &["display", "numeric"]);
        let (speed_box, speed_label) =
            Self::metric_column("Speed", "— km/h", &["title-1", "numeric"]);
        hero_grid.attach(&gradient_box, 0, 0, 1, 1);
        hero_grid.attach(&power_box, 1, 0, 1, 1);
        hero_grid.attach(&speed_box, 2, 0, 1, 1);
        inner.append(&hero_grid);

        // ── Zone ribbon ──────────────────────────────────────────────────────
        let zone_meter = ZoneMeter::new(ftp_watts);
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
        let (climb_box, climb_label) =
            Self::metric_column("Climbed", "— m", &["title-2", "numeric"]);
        secondary_grid.attach(&hr_box, 0, 0, 1, 1);
        secondary_grid.attach(&cadence_box, 1, 0, 1, 1);
        secondary_grid.attach(&elapsed_box, 2, 0, 1, 1);
        secondary_grid.attach(&climb_box, 3, 0, 1, 1);
        inner.append(&secondary_grid);

        // ── Map and profile — the slot the workout screen gives its graph ────
        // Both span the full width: the map takes the height, since "where am I"
        // is what the rider looks at mid-ride, with the profile as a strip
        // beneath answering "what is coming". Full-width rows stay legible at any
        // window size, so this needs no breakpoint.
        let route_map = RouteMap::new();
        route_map.widget().set_vexpand(true);
        elevation_chart.set_content_height(90);

        let graph_column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .vexpand(true)
            .build();
        graph_column.append(route_map.widget());
        graph_column.append(&elevation_chart);
        inner.append(&graph_column);

        inner.append(&progress_bar);
        inner.append(&time_dist_row);

        // ── Controls ─────────────────────────────────────────────────────────
        let controls = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .hexpand(true)
            .build();

        let pause_btn = gtk::Button::builder()
            .icon_name("media-playback-pause-symbolic")
            .tooltip_text("Pause ride")
            .css_classes(["circular", "flat"])
            .build();
        let end_btn = gtk::Button::builder()
            .label("End Ride")
            .css_classes(["destructive-action", "pill"])
            .tooltip_text("End the route ride and save the session")
            .build();

        // ── Intensity dial ───────────────────────────────────────────────────
        // The same control as the workout page, applied to whatever the trainer
        // is being asked for: the climb in SIM, the power target in ERG
        // emulation. It multiplies the Trainer Difficulty baseline from
        // Preferences rather than replacing it.
        let intensity_pct = Rc::new(Cell::new(100u32));
        let intensity_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .valign(gtk::Align::Center)
            .tooltip_text("Ride intensity — scales how hard the trainer makes the road")
            .build();
        let intensity_down_btn = gtk::Button::builder()
            .icon_name("list-remove-symbolic")
            .tooltip_text("Lower ride intensity")
            .css_classes(["circular", "flat"])
            .build();
        let intensity_label = gtk::Label::builder()
            .label("100%")
            .width_chars(4)
            .css_classes(["caption", "numeric"])
            .build();
        let intensity_up_btn = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("Raise ride intensity")
            .css_classes(["circular", "flat"])
            .build();
        intensity_box.update_property(&[gtk::accessible::Property::Label(
            "Ride intensity, percent of the road as it is",
        )]);
        intensity_box.append(&intensity_down_btn);
        intensity_box.append(&intensity_label);
        intensity_box.append(&intensity_up_btn);

        controls.append(&pause_btn);
        controls.append(&intensity_box);
        controls.append(&gtk::Box::builder().hexpand(true).build());
        controls.append(&end_btn);
        inner.append(&controls);

        clamp.set_child(Some(&inner));
        scroll.set_child(Some(&clamp));
        root.append(&scroll);

        // Wire buttons through replaceable callbacks
        let end_cb: ButtonCb = Rc::new(RefCell::new(None));
        let pause_cb: ButtonCb = Rc::new(RefCell::new(None));

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

        // The dial's state lives on the page rather than in a per-ride engine, so
        // these wire straight through instead of via the replaceable callbacks
        // that pause and end need.
        let dial = {
            let intensity_pct = Rc::clone(&intensity_pct);
            let intensity_label = intensity_label.clone();
            move |delta: i32| {
                let next = (intensity_pct.get() as i32 + delta)
                    .clamp(MIN_INTENSITY_PCT as i32, MAX_INTENSITY_PCT as i32)
                    as u32;
                intensity_pct.set(next);
                intensity_label.set_label(&format!("{next}%"));
            }
        };
        {
            let dial = dial.clone();
            intensity_down_btn.connect_clicked(move |_| dial(-INTENSITY_STEP_PCT));
        }
        {
            let dial = dial.clone();
            intensity_up_btn.connect_clicked(move |_| dial(INTENSITY_STEP_PCT));
        }
        Self::add_intensity_shortcuts(&root, dial);

        Self {
            root,
            countdown_banner,
            dropout_banner,
            power_countdown: Rc::new(Cell::new(0)),
            start_now_cb,
            route_name_label,
            mode_label,
            devices_section,
            devices_flow,
            power_label,
            hr_label,
            cadence_label,
            climb_label,
            gradient_label,
            speed_label,
            elapsed_label,
            dist_remaining_label,
            progress_bar,
            zone_meter,
            route_map,
            elevation_chart,
            pause_btn,
            end_btn,
            intensity_pct,
            intensity_label,
            last_readings: Rc::new(RefCell::new(ReadingsTracker::default())),
            end_cb,
            pause_cb,
            playhead_dist,
            ele_pts: ele_pts_rc,
            live_session: RefCell::new(None),
        }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// Bind `+` and `−` (and their keypad twins) to the intensity dial.
    ///
    /// Global scope with a mapped check, as on the workout page: the rider
    /// reaches this page by starting a ride, so focus is not on it and a
    /// page-local shortcut would never fire.
    fn add_intensity_shortcuts(root: &gtk::Box, dial: impl Fn(i32) + Clone + 'static) {
        let controller = gtk::ShortcutController::new();
        controller.set_scope(gtk::ShortcutScope::Global);

        for (keys, delta) in [
            ("minus|KP_Subtract", -INTENSITY_STEP_PCT),
            ("plus|equal|KP_Add", INTENSITY_STEP_PCT),
        ] {
            let Some(trigger) = gtk::ShortcutTrigger::parse_string(keys) else {
                tracing::warn!("Could not parse intensity shortcut '{keys}'");
                continue;
            };
            let dial = dial.clone();
            let action = gtk::CallbackAction::new(move |widget, _| {
                if !widget.is_mapped() {
                    return glib::Propagation::Proceed;
                }
                dial(delta);
                glib::Propagation::Stop
            });
            controller.add_shortcut(gtk::Shortcut::new(Some(trigger), Some(action)));
        }

        root.add_controller(controller);
    }

    /// A snapshot of the ride in progress, or `None` when no route ride is
    /// running. Used by the checkpoint timer; cloned rather than lent out so no
    /// borrow is held across the database write.
    pub fn live_session_snapshot(&self) -> Option<Session> {
        self.live_session
            .borrow()
            .as_ref()
            .map(|s| s.borrow().clone())
    }

    pub fn set_readings(&self, readings: LiveReadings) {
        self.last_readings
            .borrow_mut()
            .merge(readings, std::time::Instant::now());
    }

    /// Show a chip for a newly connected device. Repeat calls for the same
    /// `address` are ignored, so reconnections do not stack up chips.
    pub fn add_connected_device(&self, address: &str, display_name: &str) {
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
        chip.update_property(&[gtk::accessible::Property::Label(&format!(
            "{display_name} connected"
        ))]);

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
            }
            child = next;
        }
        if self.devices_flow.first_child().is_none() {
            self.devices_section.set_visible(false);
        }
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

    /// Reset the page for a new route.
    pub fn reset_route(&self, route: &Route) {
        self.route_name_label.set_label(&route.name);
        self.mode_label.set_label("");
        self.countdown_banner
            .set_title("Connect a power meter or trainer to begin");
        self.countdown_banner.set_revealed(true);
        self.dropout_banner.set_revealed(false);
        self.power_countdown.set(0);
        let total_dist_km = route.total_distance_m / 1000.0;
        self.dist_remaining_label
            .set_label(&format!("{total_dist_km:.1} km remaining"));
        self.progress_bar.set_fraction(0.0);
        self.elapsed_label.set_label("0:00");
        self.power_label.set_label("— W");
        self.hr_label.set_label("— bpm");
        self.cadence_label.set_label("— rpm");
        self.climb_label.set_label("— m");
        self.gradient_label.set_label("— %");
        self.speed_label.set_label("— km/h");
        self.playhead_dist.set(0.0);
        self.pause_btn
            .set_icon_name("media-playback-pause-symbolic");
        self.pause_btn.set_tooltip_text(Some("Pause ride"));
        // The dial is a judgement about today's legs, so each ride starts from
        // the Preferences baseline rather than inheriting the last ride's.
        self.intensity_pct.set(100);
        self.intensity_label.set_label("100%");

        // Rebuild the profile and hand the map the new route line.
        *self.ele_pts.borrow_mut() = profile_samples(route);
        self.elevation_chart.queue_draw();
        self.zone_meter.set_power(None);
        let line: Vec<(f64, f64)> = route.points.iter().map(|p| (p.lat, p.lng)).collect();
        self.route_map.set_route(&line);
    }

    /// Start the 1 Hz ride loop. Calls `on_complete` with the session when the route
    /// is finished or the user ends the ride.
    ///
    /// The ride does not begin until 10 consecutive seconds of power data arrive
    /// (or the rider presses "Start now" on the countdown banner).
    ///
    /// `sim_difficulty` (0.0–1.0) scales the road gradient before it reaches the
    /// trainer and `sim_max_grade` (percent) caps it; both are read live each tick
    /// so a change in Preferences applies mid-ride.
    #[allow(clippy::too_many_arguments)]
    pub fn start_timer(
        page: Rc<Self>,
        route: Route,
        athlete: &AthleteProfile,
        cmd_tx: async_channel::Sender<DeviceCommand>,
        sim_capable: Rc<Cell<bool>>,
        sim_difficulty: Rc<Cell<f32>>,
        sim_max_grade: Rc<Cell<f32>>,
        on_complete: impl Fn(Session) + 'static,
        timer_alive: Rc<Cell<bool>>,
    ) {
        timer_alive.set(true);
        let on_complete: Rc<dyn Fn(Session)> = Rc::new(on_complete);
        let completed = Rc::new(Cell::new(false));
        // False until the pre-start countdown completes — the clock, the route
        // position and the trainer commands all wait for it.
        let started = Rc::new(Cell::new(false));

        // Read once at ride start rather than at page construction, so a profile
        // edit made between launching the app and starting the ride applies.
        let mass_kg = athlete.weight_kg;
        page.zone_meter.set_ftp(athlete.ftp_watts);

        // The route names the activity, so it appears in the calendar and history
        // as the ride it was rather than as an unstructured session.
        let mut new_session = Session::new(None);
        new_session.title = Some(route.name.clone());
        // Stamp the executing FTP exactly as WorkoutEngine::new does. Without it a
        // route ride scores against whatever the profile says later, and there is
        // no way to tell an unstamped route ride from a pre-phase-1 one.
        new_session.ftp_watts = Some(athlete.ftp_watts);
        let engine = Rc::new(RefCell::new(RouteEngine::new(route, 6.944, mass_kg)));
        let session = Rc::new(RefCell::new(new_session));
        // Publish it so the checkpoint timer can reach this ride. Gated on
        // route_timer_alive there, so a finished ride is never re-written.
        *page.live_session.borrow_mut() = Some(Rc::clone(&session));
        let paused = Rc::new(Cell::new(false));
        let elapsed_secs = Rc::new(Cell::new(0u32));
        // Grade last sent to the trainer — SIM commands go out only on a real
        // change (or as a periodic keepalive), not every tick.
        let last_sent_grade = Rc::new(Cell::new(f32::NAN));

        // ── "Start now" — skip the pre-start wait ────────────────────────────
        {
            let started_btn = Rc::clone(&started);
            let banner = page.countdown_banner.clone();
            let countdown = Rc::clone(&page.power_countdown);
            let session_btn = Rc::clone(&session);
            *page.start_now_cb.borrow_mut() = Some(Box::new(move || {
                started_btn.set(true);
                countdown.set(0);
                banner.set_revealed(false);
                stamp_start(&session_btn);
            }));
        }

        // ── Pause / resume ───────────────────────────────────────────────────
        {
            let pause_btn = page.pause_btn.clone();
            let paused_c = Rc::clone(&paused);
            *page.pause_cb.borrow_mut() = Some(Box::new(move || {
                let is_paused = !paused_c.get();
                paused_c.set(is_paused);
                if is_paused {
                    pause_btn.set_icon_name("media-playback-start-symbolic");
                    pause_btn.set_tooltip_text(Some("Resume ride"));
                } else {
                    pause_btn.set_icon_name("media-playback-pause-symbolic");
                    pause_btn.set_tooltip_text(Some("Pause ride"));
                }
            }));
        }

        // ── End Ride ─────────────────────────────────────────────────────────
        {
            let end_btn = page.end_btn.clone();
            let session_end = Rc::clone(&session);
            let on_complete_end = Rc::clone(&on_complete);
            let completed_end = Rc::clone(&completed);
            let timer_alive_end = Rc::clone(&timer_alive);
            *page.end_cb.borrow_mut() = Some(Box::new(move || {
                let dialog = adw::AlertDialog::builder()
                    .heading("End Ride?")
                    .body("Your progress so far will be saved.")
                    .build();
                dialog.add_response("cancel", "_Cancel");
                dialog.add_response("end", "_End Ride");
                dialog.set_response_appearance("end", adw::ResponseAppearance::Destructive);
                dialog.set_default_response(Some("cancel"));
                dialog.set_close_response("cancel");

                let session_r = Rc::clone(&session_end);
                let on_complete_r = Rc::clone(&on_complete_end);
                let completed_r = Rc::clone(&completed_end);
                let timer_r = Rc::clone(&timer_alive_end);
                dialog.connect_response(None, move |_, resp| {
                    if resp == "end" && !completed_r.get() {
                        completed_r.set(true);
                        timer_r.set(false);
                        let mut sess = session_r.borrow().clone();
                        sess.ended_at = Some(chrono::Utc::now());
                        on_complete_r(sess);
                    }
                });
                dialog.present(Some(&end_btn));
            }));
        }

        // ── 1 Hz tick ────────────────────────────────────────────────────────
        let readings_rc = Rc::clone(&page.last_readings);
        // Sensors already reported as dropped, so each is logged once per ride.
        let stale_warned: Rc<RefCell<std::collections::HashSet<&'static str>>> =
            Rc::new(RefCell::new(std::collections::HashSet::new()));
        let timer_alive_tick = Rc::clone(&timer_alive);
        let completed_tick = Rc::clone(&completed);
        let started_tick = Rc::clone(&started);
        let power_countdown = Rc::clone(&page.power_countdown);

        glib::timeout_add_local(Duration::from_secs(1), move || {
            if !timer_alive_tick.get() {
                return glib::ControlFlow::Break;
            }

            // Only sensors that are still transmitting contribute; a strap that
            // has gone quiet must not have its last value recorded all ride.
            let now = std::time::Instant::now();
            let readings = readings_rc.borrow().current(now);
            let stale = readings_rc.borrow().stale_sensors(now);
            for sensor in &stale {
                if stale_warned.borrow_mut().insert(*sensor) {
                    tracing::warn!("{sensor} sensor stopped reporting — dropping it from the ride");
                }
            }
            page.set_stale_sensors(&stale);

            if paused.get() {
                return glib::ControlFlow::Continue;
            }

            // ── Pre-start: count consecutive seconds of power data ───────────
            if !started_tick.get() {
                if readings.power_watts.is_some_and(|w| w > 0) {
                    let n = power_countdown.get() + 1;
                    power_countdown.set(n);
                    if n >= PRE_START_SECS {
                        started_tick.set(true);
                        power_countdown.set(0);
                        page.countdown_banner.set_revealed(false);
                        stamp_start(&session);
                    }
                } else {
                    power_countdown.set(0);
                }

                if !started_tick.get() {
                    let n = power_countdown.get();
                    let title = if n == 0 {
                        "Start pedalling to begin".to_string()
                    } else {
                        format!("Starting in {} s…", PRE_START_SECS.saturating_sub(n))
                    };
                    page.countdown_banner.set_title(&title);
                    return glib::ControlFlow::Continue;
                }
            }

            let elapsed = elapsed_secs.get() + 1;
            elapsed_secs.set(elapsed);

            // SIM when a controllable trainer is connected (checked live, so a
            // mid-ride trainer drop falls back to ERG emulation gracefully);
            // otherwise the original fixed-speed ERG-target emulation.
            // The rider's live dial, read each tick so a press applies at once.
            // It multiplies the Preferences baseline rather than replacing it.
            let dial = page.intensity_pct.get() as f32 / 100.0;

            let sim = sim_capable.get();
            let speed_ms = if sim {
                let power = readings.power_watts.unwrap_or(0);
                let speed_ms = engine.borrow_mut().tick_sim(power);
                // The rate-limited grade, scaled by the rider's difficulty setting
                // and their live dial, and capped at their trainer's usable
                // maximum. Virtual speed still comes from the true gradient —
                // both change how the climb feels, not how fast the route goes by.
                let max_grade = sim_max_grade.get();
                let send_grade =
                    (engine.borrow().trainer_grade_percent() * sim_difficulty.get() * dial)
                        .clamp(-max_grade, max_grade);
                // Send the grade on a ≥0.1% change, with a 5 s keepalive.
                let last = last_sent_grade.get();
                if (send_grade - last).abs() >= 0.1 || last.is_nan() || elapsed.is_multiple_of(5) {
                    let _ = cmd_tx.try_send(DeviceCommand::SetSimulation {
                        grade_percent: send_grade,
                    });
                    last_sent_grade.set(send_grade);
                }
                speed_ms
            } else {
                // No power and no speed sensor means the rider is not riding, so
                // the route does not advance — see RouteEngine::emulated_speed_ms.
                let speed_ms =
                    RouteEngine::emulated_speed_ms(readings.power_watts, readings.speed_kmh);
                engine.borrow_mut().set_speed(speed_ms);
                // The trainer cannot follow the road here, so the dial scales the
                // power target instead of the climb — the same intent either way.
                let target_watts = (engine.borrow_mut().tick() as f32 * dial).round() as u32;
                // Send power target to trainer (clamped per CLAUDE.md §5.1)
                if target_watts > 0 {
                    let watts = target_watts.min(1000) as u16;
                    let _ = cmd_tx.try_send(DeviceCommand::SetTargetPower { watts });
                }
                speed_ms
            };

            let gradient_pct = engine.borrow().current_gradient() * 100.0;
            let distance_m = engine.borrow().distance_m;
            let total_dist = engine.borrow().route.total_distance_m;
            let is_done = engine.borrow().is_done();
            // The rider's position on the course, recorded so the ride exports as a
            // mapped activity rather than a stationary indoor session.
            let position = engine.borrow().current_position();
            let altitude_m = engine.borrow().current_elevation();

            // Record data point
            session.borrow_mut().data_points.push(DataPoint {
                elapsed_secs: elapsed,
                power_watts: readings.power_watts,
                target_watts: None,
                heart_rate_bpm: readings.heart_rate_bpm,
                cadence_rpm: readings.cadence_rpm,
                speed_kmh: Some(speed_ms * 3.6),
                lat: position.map(|(lat, _)| lat),
                lng: position.map(|(_, lng)| lng),
                altitude_m: Some(altitude_m),
            });

            // ── Update UI ────────────────────────────────────────────────────
            page.power_label.set_label(&format!(
                "{} W",
                readings
                    .power_watts
                    .map(|w| w.to_string())
                    .unwrap_or_else(|| "—".into())
            ));
            page.hr_label.set_label(&format!(
                "{} bpm",
                readings
                    .heart_rate_bpm
                    .map(|h| h.to_string())
                    .unwrap_or_else(|| "—".into())
            ));
            page.cadence_label.set_label(&format!(
                "{} rpm",
                readings
                    .cadence_rpm
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "—".into())
            ));
            page.climb_label.set_label(&format!(
                "{:.0} m",
                session.borrow().elevation_gain_m().unwrap_or(0.0)
            ));
            let mode_text = if sim {
                "SIM · resistance follows the road"
            } else {
                "ERG · fixed-speed power targets"
            };
            if page.mode_label.label() != mode_text {
                page.mode_label.set_label(mode_text);
            }
            page.gradient_label
                .set_label(&format!("{gradient_pct:+.1}%"));
            page.speed_label
                .set_label(&format!("{:.1} km/h", speed_ms * 3.6));

            let mins = elapsed / 60;
            let secs = elapsed % 60;
            page.elapsed_label.set_label(&format!("{mins}:{secs:02}"));

            let remaining_km = (total_dist - distance_m).max(0.0) / 1000.0;
            page.dist_remaining_label
                .set_label(&format!("{remaining_km:.1} km remaining"));

            if total_dist > 0.0 {
                page.progress_bar
                    .set_fraction((distance_m / total_dist) as f64);
            }

            page.playhead_dist.set(distance_m);
            page.elevation_chart.queue_draw();
            page.zone_meter.set_power(readings.power_watts);
            if let Some((lat, lng)) = position {
                page.route_map.set_position(lat, lng);
            }

            if is_done && !completed_tick.get() {
                completed_tick.set(true);
                timer_alive_tick.set(false);
                let mut sess = session.borrow().clone();
                sess.ended_at = Some(chrono::Utc::now());
                on_complete(sess);
                return glib::ControlFlow::Break;
            }

            glib::ControlFlow::Continue
        });
    }

    /// A centred caption-over-value column, as used on the workout screen.
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

    #[allow(dead_code)]
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
