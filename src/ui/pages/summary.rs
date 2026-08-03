use adw::prelude::*;
use gtk::gio;
use std::cell::RefCell;
use std::rc::Rc;

use crate::data::athlete::{AthleteProfile, ZONE_COLORS};
use crate::data::session::Session;
use crate::data::workout::{Segment, Workout, WorkoutCategory};
use crate::training::engine::WorkoutEngine;
use crate::ui::widgets::workout_graph::WorkoutGraph;
use crate::ui::widgets::zone_meter::ZONE_LABELS;

#[derive(Clone)]
pub struct SummaryPage {
    root: gtk::Box,
    /// Hero icon — a star by default, the RPE emoticon once the rider rates.
    rpe_image: gtk::Image,
    workout_name_label: gtk::Label,
    dur_label: gtk::Label,
    tss_label: gtk::Label,
    if_label: gtk::Label,
    avg_label: gtk::Label,
    np_label: gtk::Label,
    max_power_label: gtk::Label,
    kj_label: gtk::Label,
    distance_label: gtk::Label,
    climb_label: gtk::Label,
    avg_hr_label: gtk::Label,
    max_hr_label: gtk::Label,
    cadence_label: gtk::Label,
    /// Holds the ride graph (profile + actual trace), rebuilt per session.
    graph_holder: gtk::Box,
    last_session: Rc<RefCell<Option<Session>>>,
    /// The profile the last summary was drawn against — the FIT export reads
    /// FTP and heart-rate limits from it to derive training load.
    last_athlete: Rc<RefCell<AthleteProfile>>,
    export_banner: adw::Banner,
    zone_section: gtk::Box,
    zone_seconds: Rc<RefCell<[u32; 7]>>,
    zone_bar: gtk::DrawingArea,
    zone_legend: gtk::Box,
    compliance_section: gtk::Box,
    compliance_group: adw::PreferencesGroup,
    /// Tracks rows added to compliance_group so they can be removed cleanly.
    /// AdwPreferencesGroup's first_child() returns internal layout widgets, so
    /// we must remove only the rows we explicitly added.
    compliance_rows: Rc<RefCell<Vec<adw::ActionRow>>>,
}

impl SummaryPage {
    pub fn new(on_done: impl Fn() + 'static) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();

        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();

        let clamp = adw::Clamp::builder()
            .maximum_size(900)
            .margin_top(24)
            .margin_bottom(24)
            .margin_start(24)
            .margin_end(24)
            .build();

        let inner = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .build();

        // ── Hero: the ride's name and its score ──────────────────────────────
        let rpe_image = gtk::Image::builder()
            .icon_name("starred-symbolic")
            .pixel_size(48)
            .css_classes(["accent"])
            .build();
        inner.append(&rpe_image);

        inner.append(
            &gtk::Label::builder()
                .label("Workout complete")
                .halign(gtk::Align::Center)
                .css_classes(["caption", "dim-label"])
                .build(),
        );

        let workout_name_label = gtk::Label::builder()
            .label("")
            .halign(gtk::Align::Center)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["title-1"])
            .build();
        inner.append(&workout_name_label);

        // Duration | TSS (the score, display class) | IF
        let hero_grid = gtk::Grid::builder()
            .column_spacing(12)
            .column_homogeneous(true)
            .build();
        let (dur_box, dur_label) = Self::metric_column("Duration", "—", &["title-1", "numeric"]);
        let (tss_box, tss_label) = Self::metric_column("TSS", "—", &["display", "numeric"]);
        let (if_box, if_label) = Self::metric_column("Intensity", "—", &["title-1", "numeric"]);
        hero_grid.attach(&dur_box, 0, 0, 1, 1);
        hero_grid.attach(&tss_box, 1, 0, 1, 1);
        hero_grid.attach(&if_box, 2, 0, 1, 1);
        inner.append(&hero_grid);

        // ── The ride: workout profile with the actual power trace over it ────
        let graph_holder = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .visible(false)
            .build();
        inner.append(&graph_holder);

        // ── Secondary totals strip ───────────────────────────────────────────
        let totals_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(18)
            .halign(gtk::Align::Center)
            .build();
        let make_total_pair = |name: &str, tooltip: &str| -> gtk::Label {
            let pair = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(6)
                .tooltip_text(tooltip)
                .build();
            pair.append(
                &gtk::Label::builder()
                    .label(name)
                    .css_classes(["caption", "dim-label"])
                    .build(),
            );
            let value = gtk::Label::builder()
                .label("—")
                .css_classes(["caption", "numeric"])
                .build();
            pair.append(&value);
            totals_row.append(&pair);
            value
        };
        let avg_label = make_total_pair("Avg", "Average power");
        let np_label = make_total_pair("NP", "Normalised power");
        let max_power_label = make_total_pair("Max", "Peak power");
        let kj_label = make_total_pair("Energy", "Total work in kilojoules");
        inner.append(&totals_row);

        // ── Ride totals: distance, climbing and the body's side of the effort ──
        // A second row so the power figures above stay together as one group.
        let ride_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(18)
            .halign(gtk::Align::Center)
            .build();
        let make_ride_pair = |name: &str, tooltip: &str| -> gtk::Label {
            let pair = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(6)
                .tooltip_text(tooltip)
                .build();
            pair.append(
                &gtk::Label::builder()
                    .label(name)
                    .css_classes(["caption", "dim-label"])
                    .build(),
            );
            let value = gtk::Label::builder()
                .label("—")
                .css_classes(["caption", "numeric"])
                .build();
            pair.append(&value);
            ride_row.append(&pair);
            value
        };
        let distance_label = make_ride_pair("Distance", "Distance covered");
        let climb_label = make_ride_pair("Climbed", "Total elevation gain");
        let avg_hr_label = make_ride_pair("Avg HR", "Average heart rate");
        let max_hr_label = make_ride_pair("Max HR", "Peak heart rate");
        let cadence_label = make_ride_pair("Cadence", "Average cadence while pedalling");
        inner.append(&ride_row);

        // ── Zone breakdown ────────────────────────────────────────────────────
        let zone_seconds: Rc<RefCell<[u32; 7]>> = Rc::new(RefCell::new([0u32; 7]));
        let zone_bar = gtk::DrawingArea::builder()
            .content_height(20)
            .hexpand(true)
            .build();

        let zones_ref = Rc::clone(&zone_seconds);
        zone_bar.set_draw_func(move |_widget, cr, width, height| {
            let zones = zones_ref.borrow();
            let total: u32 = zones.iter().sum();
            if total == 0 {
                return;
            }
            let w = width as f64;
            let h = height as f64;
            let mut x = 0.0f64;
            for (i, &secs) in zones.iter().enumerate() {
                if secs == 0 {
                    continue;
                }
                let seg_w = (secs as f64 / total as f64) * w;
                let (r, g, b) = ZONE_COLORS[i];
                cr.set_source_rgba(r, g, b, 0.85);
                cr.rectangle(x, 0.0, seg_w, h);
                cr.fill().ok();
                x += seg_w;
            }
        });

        // Legend rebuilt per session: only the zones actually ridden, with
        // their time — an evenly-spaced Z1–Z7 row under a proportional bar
        // would mislabel the segments.
        let zone_legend = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .halign(gtk::Align::Center)
            .build();

        let zone_section = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .visible(false)
            .build();
        zone_section.append(
            &gtk::Label::builder()
                .label("Time in Zone")
                .halign(gtk::Align::Start)
                .css_classes(["heading"])
                .build(),
        );
        zone_section.append(&zone_bar);
        zone_section.append(&zone_legend);
        inner.append(&zone_section);

        // ── Interval Compliance ───────────────────────────────────────────────
        let compliance_group = adw::PreferencesGroup::builder()
            .title("Interval Compliance")
            .build();

        let compliance_section = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .visible(false)
            .build();
        compliance_section.append(&compliance_group);
        inner.append(&compliance_section);

        // ── FIT export ────────────────────────────────────────────────────────
        let last_session: Rc<RefCell<Option<Session>>> = Rc::new(RefCell::new(None));
        // The export needs FTP and heart-rate limits to work out training load,
        // so the profile behind the last summary is kept alongside the session.
        let last_athlete: Rc<RefCell<AthleteProfile>> =
            Rc::new(RefCell::new(AthleteProfile::default()));

        let export_banner = adw::Banner::builder()
            .title("")
            .button_label("Open folder")
            .build();
        export_banner.set_revealed(false);

        let export_btn = gtk::Button::builder()
            .label("Export FIT File")
            .css_classes(["flat", "pill"])
            .tooltip_text("Export this session as a .FIT file")
            .build();

        let folder_uri: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let folder_uri_for_btn = Rc::clone(&folder_uri);
        let export_banner_btn = export_banner.clone();
        export_banner_btn.connect_button_clicked(move |_| {
            let uri = folder_uri_for_btn.borrow().clone();
            if !uri.is_empty() {
                let file = gio::File::for_parse_name(&uri);
                gtk::FileLauncher::new(Some(&file)).launch(
                    None::<&gtk::Window>,
                    None::<&gio::Cancellable>,
                    |_| {},
                );
            }
        });

        let session_for_export = Rc::clone(&last_session);
        let athlete_for_export = Rc::clone(&last_athlete);
        let banner_ref = export_banner.clone();
        export_btn.connect_clicked(move |_| {
            let session = session_for_export.borrow().clone();
            let Some(session) = session else { return };
            let athlete = athlete_for_export.borrow().clone();
            match crate::data::fit::export_to_xdg_path(&session, &athlete) {
                Ok(path) => {
                    if let Some(parent) = path.parent() {
                        *folder_uri.borrow_mut() = format!("file://{}", parent.display());
                    }
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    banner_ref.set_title(&format!("Exported: {}", name));
                    banner_ref.set_revealed(true);
                }
                Err(e) => {
                    banner_ref.set_title(&format!("Export failed: {}", e));
                    banner_ref.set_revealed(true);
                }
            }
        });

        inner.append(&export_banner);

        // ── Actions row ───────────────────────────────────────────────────────
        let done_btn = gtk::Button::builder()
            .label("Back to Dashboard")
            .css_classes(["suggested-action", "pill"])
            .tooltip_text("Return to the dashboard")
            .build();
        done_btn.connect_clicked(move |_| on_done());

        let actions_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .halign(gtk::Align::Center)
            .build();
        actions_row.append(&export_btn);
        actions_row.append(&done_btn);
        inner.append(&actions_row);

        clamp.set_child(Some(&inner));
        scroll.set_child(Some(&clamp));
        root.append(&scroll);

        Self {
            root,
            rpe_image,
            workout_name_label,
            dur_label,
            tss_label,
            if_label,
            avg_label,
            np_label,
            max_power_label,
            kj_label,
            distance_label,
            climb_label,
            avg_hr_label,
            max_hr_label,
            cadence_label,
            graph_holder,
            last_session,
            last_athlete,
            export_banner,
            zone_section,
            zone_seconds,
            zone_bar,
            zone_legend,
            compliance_section,
            compliance_group,
            compliance_rows: Rc::new(RefCell::new(Vec::new())),
        }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// A centred caption-over-value column, as on the player and dashboard.
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

    /// Small zone-coloured square for the legend.
    fn zone_swatch(zone_idx: usize) -> gtk::DrawingArea {
        let area = gtk::DrawingArea::builder()
            .content_width(12)
            .content_height(12)
            .valign(gtk::Align::Center)
            .build();
        area.set_draw_func(move |_widget, cr, width, height| {
            let (r, g, b) = ZONE_COLORS[zone_idx];
            cr.set_source_rgba(r, g, b, 0.85);
            cr.rectangle(0.0, 0.0, width as f64, height as f64);
            cr.fill().ok();
        });
        area
    }

    /// Show the bundled RPE emoticon icon in the hero.
    pub fn show_rpe_icon(&self, rpe: u8) {
        if let Some(texture) = crate::ui::resources::rpe_texture(rpe) {
            self.rpe_image.set_paintable(Some(&texture));
        }
    }

    /// Populate stat labels, ride graph, zone breakdown, and interval
    /// compliance from a completed session.
    pub fn update(
        &self,
        session: &Session,
        workout_name: &str,
        athlete: &AthleteProfile,
        segments: Option<&[Segment]>,
    ) {
        let ftp = athlete.ftp_watts;
        *self.last_session.borrow_mut() = Some(session.clone());
        *self.last_athlete.borrow_mut() = athlete.clone();
        self.export_banner.set_revealed(false);
        // Reset the hero icon — the RPE emoticon belongs to the previous ride.
        self.rpe_image.set_icon_name(Some("starred-symbolic"));

        self.workout_name_label.set_label(workout_name);

        let dur = session.duration_secs() as u32;
        self.dur_label
            .set_label(&WorkoutEngine::format_duration(dur));

        match session.tss(ftp) {
            Some(t) => self.tss_label.set_label(&format!("{}", t as u32)),
            None => self.tss_label.set_label("—"),
        }

        let np = session.normalised_power();
        match np {
            Some(p) => self.if_label.set_label(&format!("{:.2}", p / ftp as f32)),
            None => self.if_label.set_label("—"),
        }

        match session.average_power() {
            Some(p) => self.avg_label.set_label(&format!("{} W", p as u32)),
            None => self.avg_label.set_label("—"),
        }
        match np {
            Some(p) => self.np_label.set_label(&format!("{} W", p as u32)),
            None => self.np_label.set_label("—"),
        }
        match session.max_power() {
            Some(p) => self.max_power_label.set_label(&format!("{p} W")),
            None => self.max_power_label.set_label("—"),
        }
        self.kj_label
            .set_label(&format!("{:.0} kJ", session.kilojoules()));

        let distance_km = session.distance_m() / 1000.0;
        if distance_km > 0.0 {
            self.distance_label
                .set_label(&format!("{distance_km:.1} km"));
        } else {
            self.distance_label.set_label("—");
        }
        match session.elevation_gain_m() {
            Some(g) => self.climb_label.set_label(&format!("{g:.0} m")),
            None => self.climb_label.set_label("—"),
        }
        match session.average_hr() {
            Some(h) => self.avg_hr_label.set_label(&format!("{} bpm", h as u32)),
            None => self.avg_hr_label.set_label("—"),
        }
        match session.max_hr() {
            Some(h) => self.max_hr_label.set_label(&format!("{h} bpm")),
            None => self.max_hr_label.set_label("—"),
        }
        match session.average_cadence() {
            Some(c) => self.cadence_label.set_label(&format!("{} rpm", c as u32)),
            None => self.cadence_label.set_label("—"),
        }

        // ── Ride graph: the workout profile with the actual trace over it ────
        while let Some(child) = self.graph_holder.first_child() {
            self.graph_holder.remove(&child);
        }
        let total_secs: u32 = segments
            .map(|segs| segs.iter().map(|s| s.duration_secs).sum())
            .unwrap_or(0);
        if let (Some(segs), true) = (segments, total_secs > 0) {
            // Synthetic workout/athlete carry just what the graph draws:
            // the segment profile and the FTP scale.
            let workout = Workout {
                id: 0,
                name: String::new(),
                description: String::new(),
                duration_secs: total_secs,
                tss: 0.0,
                category: WorkoutCategory::Custom,
                segments: segs.to_vec(),
            };
            let athlete = AthleteProfile {
                ftp_watts: ftp.max(1),
                ..AthleteProfile::default()
            };
            let graph = WorkoutGraph::new(&workout, &athlete);
            graph.widget().set_content_height(160);

            let mut trace: Vec<Option<u32>> = vec![None; total_secs as usize];
            for dp in &session.data_points {
                if let Some(slot) = trace.get_mut(dp.elapsed_secs as usize) {
                    *slot = dp.power_watts;
                }
            }
            graph.set_trace(trace);
            self.graph_holder.append(graph.widget());
            self.graph_holder.set_visible(true);
        } else {
            self.graph_holder.set_visible(false);
        }

        // ── Zone breakdown ────────────────────────────────────────────────────
        let zone_secs = session.time_in_zones(ftp);
        let has_power = zone_secs.iter().any(|&s| s > 0);
        self.zone_section.set_visible(has_power);
        if has_power {
            *self.zone_seconds.borrow_mut() = zone_secs;
            self.zone_bar.queue_draw();

            // Legend: only the zones actually ridden, with their time.
            while let Some(child) = self.zone_legend.first_child() {
                self.zone_legend.remove(&child);
            }
            for (i, &secs) in zone_secs.iter().enumerate() {
                if secs == 0 {
                    continue;
                }
                let pair = gtk::Box::builder()
                    .orientation(gtk::Orientation::Horizontal)
                    .spacing(6)
                    .tooltip_text(ZONE_LABELS[i])
                    .build();
                pair.append(&Self::zone_swatch(i));
                pair.append(
                    &gtk::Label::builder()
                        .label(format!(
                            "Z{} {}",
                            i + 1,
                            WorkoutEngine::format_duration(secs)
                        ))
                        .css_classes(["caption", "dim-label", "numeric"])
                        .build(),
                );
                self.zone_legend.append(&pair);
            }
        }

        // ── Interval compliance ───────────────────────────────────────────────
        // Remove only the rows we added — first_child() on AdwPreferencesGroup
        // returns internal layout widgets and must not be passed to remove().
        for row in self.compliance_rows.borrow().iter() {
            self.compliance_group.remove(row);
        }
        self.compliance_rows.borrow_mut().clear();

        if let Some(segs) = segments {
            let named_segs: Vec<(usize, &Segment)> = segs
                .iter()
                .enumerate()
                .filter(|(_, s)| s.label.as_deref().is_some_and(|l| !l.trim().is_empty()))
                .collect();

            if !named_segs.is_empty() {
                let mut elapsed = 0u32;
                let mut seg_times: Vec<(u32, u32)> = Vec::new();
                for seg in segs {
                    seg_times.push((elapsed, elapsed + seg.duration_secs));
                    elapsed += seg.duration_secs;
                }

                for (seg_idx, seg) in &named_segs {
                    let (t_start, t_end) = seg_times[*seg_idx];
                    let target_pct = (seg.power_low_pct + seg.power_high_pct) / 2.0;
                    let target_watts = (target_pct / 100.0 * ftp as f32) as u32;

                    let actual_readings: Vec<u32> = session
                        .data_points
                        .iter()
                        .filter(|dp| dp.elapsed_secs >= t_start && dp.elapsed_secs < t_end)
                        .filter_map(|dp| dp.power_watts)
                        .collect();

                    let (actual_str, hit) = if actual_readings.is_empty() {
                        ("No data".to_string(), false)
                    } else {
                        let avg =
                            actual_readings.iter().sum::<u32>() / actual_readings.len() as u32;
                        let ratio = avg as f32 / target_watts.max(1) as f32;
                        let hit = ratio >= 0.92;
                        (
                            format!("{} W · {}%", avg, (ratio * 100.0).round() as u32),
                            hit,
                        )
                    };

                    let row = adw::ActionRow::builder()
                        .title(seg.label.as_deref().unwrap_or(""))
                        .subtitle(format!("Target: {} W", target_watts))
                        .build();

                    let result_lbl = gtk::Label::builder()
                        .label(&actual_str)
                        .css_classes(["numeric", if hit { "success" } else { "warning" }])
                        .valign(gtk::Align::Center)
                        .build();
                    row.add_suffix(&result_lbl);

                    self.compliance_group.add(&row);
                    self.compliance_rows.borrow_mut().push(row);
                }

                self.compliance_section.set_visible(true);
            } else {
                self.compliance_section.set_visible(false);
            }
        } else {
            self.compliance_section.set_visible(false);
        }
    }
}
