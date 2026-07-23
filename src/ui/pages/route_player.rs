use adw::prelude::*;
use gtk::glib;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use crate::data::route::Route;
use crate::data::session::{DataPoint, LiveReadings, Session};
use crate::devices::manager::DeviceCommand;
use crate::training::route_engine::RouteEngine;

type ButtonCb = Rc<RefCell<Option<Box<dyn Fn()>>>>;

pub struct RoutePlayerPage {
    root: gtk::Box,
    route_name_label: gtk::Label,
    mode_label: gtk::Label,
    power_label: gtk::Label,
    target_label: gtk::Label,
    gradient_label: gtk::Label,
    speed_label: gtk::Label,
    elapsed_label: gtk::Label,
    dist_remaining_label: gtk::Label,
    progress_bar: gtk::ProgressBar,
    elevation_chart: gtk::DrawingArea,
    pause_btn: gtk::Button,
    end_btn: gtk::Button,
    pub last_readings: Rc<RefCell<LiveReadings>>,
    end_cb: ButtonCb,
    pause_cb: ButtonCb,
    /// Current distance_m for the playhead marker on the chart.
    playhead_dist: Rc<Cell<f32>>,
    /// Elevation profile points — updated by reset_route() for each new ride.
    ele_pts: Rc<RefCell<Vec<(f32, f32)>>>,
}

impl RoutePlayerPage {
    pub fn new(route: &Route) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();

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

        // ── Elevation profile ─────────────────────────────────────────────────
        let ele_pts: Vec<(f32, f32)> = route
            .points
            .iter()
            .map(|p| (p.distance_m / 1000.0, p.elevation_m))
            .collect();
        let ele_pts_rc: Rc<RefCell<Vec<(f32, f32)>>> = Rc::new(RefCell::new(ele_pts));
        let playhead_dist = Rc::new(Cell::new(0.0f32));
        let total_dist_km = route.total_distance_m / 1000.0;

        let elevation_chart = gtk::DrawingArea::builder()
            .content_height(90)
            .hexpand(true)
            .accessible_role(gtk::AccessibleRole::Img)
            .build();
        elevation_chart.update_property(&[gtk::accessible::Property::Label(
            "Route elevation profile with current position marker",
        )]);
        {
            let pts = Rc::clone(&ele_pts_rc);
            let ph = Rc::clone(&playhead_dist);
            elevation_chart.set_draw_func(move |_w, cr, width, height| {
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

                // Fill
                let (x0, y0) = (to_x(pts[0].0), to_y(pts[0].1));
                cr.set_source_rgba(1.0, 0.75, 0.20, 0.20);
                cr.move_to(x0, h);
                cr.line_to(x0, y0);
                for p in &pts[1..] {
                    cr.line_to(to_x(p.0), to_y(p.1));
                }
                let xl = to_x(pts[pts.len() - 1].0);
                cr.line_to(xl, h);
                cr.close_path();
                cr.fill().ok();

                // Line
                cr.set_source_rgba(1.0, 0.75, 0.20, 0.85);
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
        inner.append(&elevation_chart);

        // ── Overall progress bar ──────────────────────────────────────────────
        let progress_bar = gtk::ProgressBar::builder()
            .fraction(0.0)
            .show_text(false)
            .build();
        progress_bar.add_css_class("accent");
        inner.append(&progress_bar);

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
        inner.append(&time_dist_row);

        // ── 2×2 live metric grid ──────────────────────────────────────────────
        let metrics_grid = gtk::Grid::builder()
            .row_spacing(12)
            .column_spacing(12)
            .column_homogeneous(true)
            .build();

        let (power_frame, power_label) = Self::metric_card("Power", "— W", &["accent"]);
        let (target_frame, target_label) = Self::metric_card("Target", "— W", &["accent"]);
        let (gradient_frame, gradient_label) = Self::metric_card("Gradient", "— %", &[]);
        let (speed_frame, speed_label) = Self::metric_card("Speed", "— km/h", &[]);

        metrics_grid.attach(&power_frame, 0, 0, 1, 1);
        metrics_grid.attach(&target_frame, 1, 0, 1, 1);
        metrics_grid.attach(&gradient_frame, 0, 1, 1, 1);
        metrics_grid.attach(&speed_frame, 1, 1, 1, 1);
        inner.append(&metrics_grid);

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

        controls.append(&pause_btn);
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

        Self {
            root,
            route_name_label,
            mode_label,
            power_label,
            target_label,
            gradient_label,
            speed_label,
            elapsed_label,
            dist_remaining_label,
            progress_bar,
            elevation_chart,
            pause_btn,
            end_btn,
            last_readings: Rc::new(RefCell::new(LiveReadings::default())),
            end_cb,
            pause_cb,
            playhead_dist,
            ele_pts: ele_pts_rc,
        }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    pub fn set_readings(&self, readings: LiveReadings) {
        let mut stored = self.last_readings.borrow_mut();
        if readings.power_watts.is_some() {
            stored.power_watts = readings.power_watts;
        }
        if readings.speed_kmh.is_some() {
            stored.speed_kmh = readings.speed_kmh;
        }
        if readings.heart_rate_bpm.is_some() {
            stored.heart_rate_bpm = readings.heart_rate_bpm;
        }
        if readings.cadence_rpm.is_some() {
            stored.cadence_rpm = readings.cadence_rpm;
        }
    }

    /// Reset the page for a new route.
    pub fn reset_route(&self, route: &Route) {
        self.route_name_label.set_label(&route.name);
        self.mode_label.set_label("");
        let total_dist_km = route.total_distance_m / 1000.0;
        self.dist_remaining_label
            .set_label(&format!("{total_dist_km:.1} km remaining"));
        self.progress_bar.set_fraction(0.0);
        self.elapsed_label.set_label("0:00");
        self.power_label.set_label("— W");
        self.target_label.set_label("— W");
        self.gradient_label.set_label("— %");
        self.speed_label.set_label("— km/h");
        self.playhead_dist.set(0.0);
        self.pause_btn
            .set_icon_name("media-playback-pause-symbolic");
        self.pause_btn.set_tooltip_text(Some("Pause ride"));

        // Update elevation profile data for the new route.
        *self.ele_pts.borrow_mut() = route
            .points
            .iter()
            .map(|p| (p.distance_m / 1000.0, p.elevation_m))
            .collect();
        self.elevation_chart.queue_draw();
    }

    /// Start the 1 Hz ride loop. Calls `on_complete` with the session when the route
    /// is finished or the user ends the ride.
    pub fn start_timer(
        page: Rc<Self>,
        route: Route,
        mass_kg: f32,
        cmd_tx: async_channel::Sender<DeviceCommand>,
        sim_capable: Rc<Cell<bool>>,
        on_complete: impl Fn(Session) + 'static,
        timer_alive: Rc<Cell<bool>>,
    ) {
        timer_alive.set(true);
        let on_complete: Rc<dyn Fn(Session)> = Rc::new(on_complete);
        let completed = Rc::new(Cell::new(false));

        let engine = Rc::new(RefCell::new(RouteEngine::new(route, 6.944, mass_kg)));
        let session = Rc::new(RefCell::new(Session::new(None)));
        let paused = Rc::new(Cell::new(false));
        let elapsed_secs = Rc::new(Cell::new(0u32));
        // Grade last sent to the trainer — SIM commands go out only on a real
        // change (or as a periodic keepalive), not every tick.
        let last_sent_grade = Rc::new(Cell::new(f32::NAN));

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
        let timer_alive_tick = Rc::clone(&timer_alive);
        let completed_tick = Rc::clone(&completed);

        glib::timeout_add_local(Duration::from_secs(1), move || {
            if !timer_alive_tick.get() {
                return glib::ControlFlow::Break;
            }

            let readings = readings_rc.borrow().clone();

            if paused.get() {
                return glib::ControlFlow::Continue;
            }

            let elapsed = elapsed_secs.get() + 1;
            elapsed_secs.set(elapsed);

            // SIM when a controllable trainer is connected (checked live, so a
            // mid-ride trainer drop falls back to ERG emulation gracefully);
            // otherwise the original fixed-speed ERG-target emulation.
            let sim = sim_capable.get();
            let (speed_ms, target_watts) = if sim {
                let power = readings.power_watts.unwrap_or(0);
                let speed_ms = engine.borrow_mut().tick_sim(power);
                let gradient_pct = engine.borrow().current_gradient() * 100.0;
                // Send the grade on a ≥0.1% change, with a 5 s keepalive.
                let last = last_sent_grade.get();
                if (gradient_pct - last).abs() >= 0.1 || last.is_nan() || elapsed.is_multiple_of(5)
                {
                    let _ = cmd_tx.try_send(DeviceCommand::SetSimulation {
                        grade_percent: gradient_pct,
                    });
                    last_sent_grade.set(gradient_pct);
                }
                (speed_ms, None)
            } else {
                // Advance position using speed from sensor or a default 6.944 m/s (25 km/h)
                let speed_ms = readings.speed_kmh.map(|kmh| kmh / 3.6).unwrap_or(6.944);
                engine.borrow_mut().set_speed(speed_ms);
                let target_watts = engine.borrow_mut().tick();
                // Send power target to trainer (clamped per CLAUDE.md §5.1)
                if target_watts > 0 {
                    let watts = target_watts.min(1000) as u16;
                    let _ = cmd_tx.try_send(DeviceCommand::SetTargetPower { watts });
                }
                (speed_ms, Some(target_watts))
            };

            let gradient_pct = engine.borrow().current_gradient() * 100.0;
            let distance_m = engine.borrow().distance_m;
            let total_dist = engine.borrow().route.total_distance_m;
            let is_done = engine.borrow().is_done();

            // Record data point
            session.borrow_mut().data_points.push(DataPoint {
                elapsed_secs: elapsed,
                power_watts: readings.power_watts,
                heart_rate_bpm: readings.heart_rate_bpm,
                cadence_rpm: readings.cadence_rpm,
                speed_kmh: Some(speed_ms * 3.6),
                lat: None,
                lng: None,
            });

            // ── Update UI ────────────────────────────────────────────────────
            page.power_label.set_label(&format!(
                "{} W",
                readings
                    .power_watts
                    .map(|w| w.to_string())
                    .unwrap_or_else(|| "—".into())
            ));
            match target_watts {
                Some(w) => page.target_label.set_label(&format!("{w} W")),
                None => page.target_label.set_label("— W"), // SIM: no power target
            }
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
