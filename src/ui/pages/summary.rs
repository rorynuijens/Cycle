use adw::prelude::*;
use gtk::gio;
use std::cell::RefCell;
use std::rc::Rc;

use crate::data::athlete::{power_zone_index, ZONE_COLORS};
use crate::data::session::Session;
use crate::data::workout::Segment;
use crate::training::engine::WorkoutEngine;

#[derive(Clone)]
pub struct SummaryPage {
    root: gtk::Box,
    status: adw::StatusPage,
    workout_name_label: gtk::Label,
    dur_label: gtk::Label,
    avg_label: gtk::Label,
    np_label: gtk::Label,
    if_label: gtk::Label,
    tss_label: gtk::Label,
    kj_label: gtk::Label,
    last_session: Rc<RefCell<Option<Session>>>,
    export_banner: adw::Banner,
    zone_section: gtk::Box,
    zone_seconds: Rc<RefCell<[u32; 7]>>,
    zone_bar: gtk::DrawingArea,
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

        // ── Hero ─────────────────────────────────────────────────────────────
        let workout_name_label = gtk::Label::builder()
            .label("")
            .css_classes(["dim-label", "caption"])
            .halign(gtk::Align::Center)
            .build();

        let status = adw::StatusPage::builder()
            .icon_name("starred-symbolic")
            .title("Workout Complete")
            .build();
        inner.append(&status);
        inner.append(&workout_name_label);

        // ── Stats ─────────────────────────────────────────────────────────────
        let stats_group = adw::PreferencesGroup::builder()
            .title("Session Stats")
            .build();

        let make_row = |title: &str| -> (adw::ActionRow, gtk::Label) {
            let row = adw::ActionRow::builder().title(title).build();
            let lbl = gtk::Label::builder()
                .label("—")
                .css_classes(["dim-label", "numeric"])
                .valign(gtk::Align::Center)
                .build();
            row.add_suffix(&lbl);
            (row, lbl)
        };

        let (r, dur_label) = make_row("Duration");
        stats_group.add(&r);
        let (r, avg_label) = make_row("Avg Power");
        stats_group.add(&r);
        let (r, np_label) = make_row("Normalised Power");
        stats_group.add(&r);
        let (r, if_label) = make_row("Intensity Factor");
        stats_group.add(&r);
        let (r, tss_label) = make_row("TSS");
        stats_group.add(&r);
        let (r, kj_label) = make_row("Kilojoules");
        stats_group.add(&r);

        inner.append(&stats_group);

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

        // Zone legend row (Z1–Z7 labels)
        let zone_legend = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(0)
            .homogeneous(true)
            .build();
        for label in ["Z1", "Z2", "Z3", "Z4", "Z5", "Z6", "Z7"] {
            zone_legend.append(
                &gtk::Label::builder()
                    .label(label)
                    .css_classes(["caption", "dim-label"])
                    .halign(gtk::Align::Center)
                    .build(),
            );
        }

        let zone_section = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
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

        let export_banner = adw::Banner::builder()
            .title("")
            .button_label("Open folder")
            .build();
        export_banner.set_revealed(false);

        let export_btn = gtk::Button::builder()
            .label("Export FIT File")
            .css_classes(["flat", "pill"])
            .halign(gtk::Align::Center)
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
        let banner_ref = export_banner.clone();
        export_btn.connect_clicked(move |_| {
            let session = session_for_export.borrow().clone();
            let Some(session) = session else { return };
            match crate::data::fit::export_to_xdg_path(&session) {
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

        inner.append(&export_btn);
        inner.append(&export_banner);

        // ── Done button ───────────────────────────────────────────────────────
        let done_btn = gtk::Button::builder()
            .label("Back to Dashboard")
            .css_classes(["suggested-action", "pill"])
            .halign(gtk::Align::Center)
            .tooltip_text("Return to the dashboard")
            .build();
        done_btn.connect_clicked(move |_| on_done());
        inner.append(&done_btn);

        clamp.set_child(Some(&inner));
        scroll.set_child(Some(&clamp));
        root.append(&scroll);

        Self {
            root,
            status,
            workout_name_label,
            dur_label,
            avg_label,
            np_label,
            if_label,
            tss_label,
            kj_label,
            last_session,
            export_banner,
            zone_section,
            zone_seconds,
            zone_bar,
            compliance_section,
            compliance_group,
            compliance_rows: Rc::new(RefCell::new(Vec::new())),
        }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// Show the bundled RPE emoticon icon on the status page hero.
    pub fn show_rpe_icon(&self, rpe: u8) {
        if let Some(texture) = crate::ui::resources::rpe_texture(rpe) {
            self.status.set_paintable(Some(&texture));
            self.status.set_icon_name(None);
        }
    }

    /// Populate stat labels, zone breakdown, and interval compliance from a completed session.
    pub fn update(
        &self,
        session: &Session,
        workout_name: &str,
        ftp: u32,
        segments: Option<&[Segment]>,
    ) {
        *self.last_session.borrow_mut() = Some(session.clone());
        self.export_banner.set_revealed(false);

        self.workout_name_label.set_label(workout_name);

        let dur = session.duration_secs() as u32;
        self.dur_label
            .set_label(&WorkoutEngine::format_duration(dur));

        match session.average_power() {
            Some(p) => self.avg_label.set_label(&format!("{} W", p as u32)),
            None => self.avg_label.set_label("—"),
        }

        let np = session.normalised_power();
        match np {
            Some(p) => self.np_label.set_label(&format!("{} W", p as u32)),
            None => self.np_label.set_label("—"),
        }

        match np {
            Some(p) => self.if_label.set_label(&format!("{:.2}", p / ftp as f32)),
            None => self.if_label.set_label("—"),
        }

        match session.tss(ftp) {
            Some(t) => self.tss_label.set_label(&format!("{}", t as u32)),
            None => self.tss_label.set_label("—"),
        }

        let kj = session.kilojoules();
        self.kj_label.set_label(&format!("{:.0} kJ", kj));

        // ── Zone breakdown ────────────────────────────────────────────────────
        let mut zone_secs = [0u32; 7];
        for dp in &session.data_points {
            if let Some(watts) = dp.power_watts {
                zone_secs[power_zone_index(watts, ftp)] += 1;
            }
        }
        let has_power = zone_secs.iter().any(|&s| s > 0);
        self.zone_section.set_visible(has_power);
        if has_power {
            *self.zone_seconds.borrow_mut() = zone_secs;
            self.zone_bar.queue_draw();
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
