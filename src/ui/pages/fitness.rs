use adw::prelude::*;
use async_channel;
use chrono::{Datelike, Duration, Local, NaiveDate};
use gtk::glib;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use sqlx::SqlitePool;

use crate::ai::coach::{build_fitness_prompt, get_suggestion, FitnessContext, WellnessSnapshot};
use crate::ai::retrospective::{
    build_retrospective_prompt, RetroPeriod, RetroSession, RetrospectiveContext,
};
use crate::data::{
    athlete::{power_zone_index, AthleteProfile, ZONE_COLORS},
    db, keystore,
    streams::ActivityStreams,
};
use crate::ui::markdown::to_pango;
use crate::ui::pages::coaching::normalize_sport_type;

/// Standard race distances used for the pace curve, in metres.
const PACE_DISTANCES: [f32; 8] = [
    400.0, 800.0, 1609.0, 3000.0, 5000.0, 10000.0, 21097.5, 42195.0,
];
const PACE_LABELS: [&str; 8] = [
    "400 m", "800 m", "1 mi", "3 km", "5 km", "10 km", "Half", "Full",
];

type PmcPoint = (NaiveDate, f64, f64, f64);

fn hr_zone_index(bpm: u32, max_hr: u32) -> usize {
    if max_hr == 0 {
        return 0;
    }
    match (bpm as f64 / max_hr as f64 * 100.0) as u32 {
        0..=60 => 0,
        61..=70 => 1,
        71..=80 => 2,
        81..=90 => 3,
        _ => 4,
    }
}

fn format_pace_display(sec_per_km: u32) -> String {
    format!("{}:{:02}", sec_per_km / 60, sec_per_km % 60)
}

/// Compute best pace (sec/km) per standard distance from cached run streams.
/// Returns `Vec<(all_time, thirty_day)>` with 0 = no data for that distance.
fn compute_pace_curve(
    run_streams: &[(NaiveDate, String)],
    cutoff_30d: NaiveDate,
) -> Vec<(u32, u32)> {
    let mut all_time = vec![0u32; PACE_DISTANCES.len()];
    let mut month = vec![0u32; PACE_DISTANCES.len()];
    for (date, json) in run_streams {
        let is_recent = *date >= cutoff_30d;
        if let Some(streams) = ActivityStreams::from_json(json) {
            for (i, &dist) in PACE_DISTANCES.iter().enumerate() {
                if let Some(elapsed) = streams.best_time_for_distance(dist) {
                    let pace = (elapsed as f32 * 1000.0 / dist).round() as u32;
                    if pace > 0 {
                        if all_time[i] == 0 || pace < all_time[i] {
                            all_time[i] = pace;
                        }
                        if is_recent && (month[i] == 0 || pace < month[i]) {
                            month[i] = pace;
                        }
                    }
                }
            }
        }
    }
    all_time.into_iter().zip(month).collect()
}

/// Compute seconds spent in each of 5 HR zones from local session data points.
fn compute_hr_zones(records: &[db::SessionRecord], max_hr: u32) -> [u32; 5] {
    let mut zones = [0u32; 5];
    for record in records {
        for dp in &record.session.data_points {
            if let Some(bpm) = dp.heart_rate_bpm {
                zones[hr_zone_index(bpm, max_hr)] += 1;
            }
        }
    }
    zones
}

/// Compute CTL, ATL, and CTL-4-weeks-ago in one EMA pass.
/// `intervals_pairs` contains (date, tss) from Intervals.icu activities and is merged
/// with the in-app session records before computing the EMAs.
pub(crate) fn compute_load_metrics(
    rides: &[db::SessionSummary],
    intervals_pairs: &[(NaiveDate, f32)],
    ftp: u32,
    today: NaiveDate,
) -> (f64, f64, f64) {
    let mut daily_tss: HashMap<NaiveDate, f32> = HashMap::new();
    for ride in rides {
        // Skip rides Intervals.icu already accounts for — their TSS arrives
        // via intervals_pairs, so counting them here too would inflate CTL/ATL
        // by double-counting the same workout.
        if ride.counted_via_intervals() {
            continue;
        }
        let date = ride.started_at.with_timezone(&Local).date_naive();
        if let Some(tss) = ride.tss(ftp) {
            *daily_tss.entry(date).or_insert(0.0) += tss;
        }
    }
    for &(date, tss) in intervals_pairs {
        *daily_tss.entry(date).or_insert(0.0) += tss;
    }

    let Some(earliest) = daily_tss.keys().min().copied() else {
        return (0.0, 0.0, 0.0);
    };

    let ctl_alpha = 1.0_f64 - (-1.0_f64 / 42.0).exp();
    let atl_alpha = 1.0_f64 - (-1.0_f64 / 7.0).exp();
    let four_wk_ago = today - Duration::weeks(4);

    let mut ctl = 0.0_f64;
    let mut atl = 0.0_f64;
    let mut ctl_4wk_ago = 0.0_f64;
    let mut date = earliest;
    loop {
        let tss = daily_tss.get(&date).copied().unwrap_or(0.0) as f64;
        ctl += ctl_alpha * (tss - ctl);
        atl += atl_alpha * (tss - atl);
        if date == four_wk_ago {
            ctl_4wk_ago = ctl;
        }
        if date == today {
            break;
        }
        match date.succ_opt() {
            Some(next) => date = next,
            None => break,
        }
    }
    (ctl, atl, ctl_4wk_ago)
}

/// Returns `(date, ctl, atl, tsb)` for each day from 90 days ago up to `today`.
/// EMA is warmed up from `earliest` available data even if that's further back.
fn compute_pmc_series(
    rides: &[db::SessionSummary],
    intervals_pairs: &[(NaiveDate, f32)],
    ftp: u32,
    today: NaiveDate,
) -> Vec<PmcPoint> {
    let mut daily_tss: HashMap<NaiveDate, f32> = HashMap::new();
    for ride in rides {
        if ride.counted_via_intervals() {
            continue;
        }
        let date = ride.started_at.with_timezone(&Local).date_naive();
        if let Some(tss) = ride.tss(ftp) {
            *daily_tss.entry(date).or_insert(0.0) += tss;
        }
    }
    for &(date, tss) in intervals_pairs {
        *daily_tss.entry(date).or_insert(0.0) += tss;
    }

    let Some(earliest) = daily_tss.keys().min().copied() else {
        return Vec::new();
    };

    let ctl_alpha = 1.0_f64 - (-1.0_f64 / 42.0).exp();
    let atl_alpha = 1.0_f64 - (-1.0_f64 / 7.0).exp();
    let window_start = today - Duration::days(90);

    let mut ctl = 0.0_f64;
    let mut atl = 0.0_f64;
    let mut series = Vec::new();
    let mut date = earliest;
    loop {
        let tss = daily_tss.get(&date).copied().unwrap_or(0.0) as f64;
        ctl += ctl_alpha * (tss - ctl);
        atl += atl_alpha * (tss - atl);
        if date >= window_start {
            series.push((date, ctl, atl, ctl - atl));
        }
        if date == today {
            break;
        }
        match date.succ_opt() {
            Some(next) => date = next,
            None => break,
        }
    }
    series
}

/// Plain-language reading of the TSB value — the hero's headline phrase.
fn tsb_status_text(tsb: f64) -> &'static str {
    if tsb > 25.0 {
        "Very fresh — consider adding volume"
    } else if tsb > 5.0 {
        "Fresh — ready for quality work"
    } else if tsb > -10.0 {
        "Normal training fatigue"
    } else if tsb > -30.0 {
        "Elevated fatigue — consider easier days"
    } else {
        "High fatigue — prioritise rest"
    }
}

pub struct FitnessPage {
    root: gtk::Box,
}

impl FitnessPage {
    /// Returns `(page, reload_fn)`. Call `reload_fn()` when the page becomes visible
    /// or after the athlete's FTP changes.
    pub fn new(
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        ftp: Rc<Cell<u32>>,
        athlete: Rc<RefCell<AthleteProfile>>,
    ) -> (Self, Rc<dyn Fn()>) {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();

        // ── API key pre-flight banner ─────────────────────────────────────────
        let api_banner = adw::Banner::builder()
            .title("Add your Anthropic API key in Preferences → Integrations to use AI features")
            .button_label("Open Preferences")
            .revealed(false)
            .build();
        root.append(&api_banner);

        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();

        // Chart-dominated page — wider clamp than the standard 900 (same
        // justification as the calendar) so the PMC and curves get usable
        // horizontal resolution.
        let clamp = adw::Clamp::builder()
            .maximum_size(1200)
            .margin_top(24)
            .margin_bottom(24)
            .margin_start(24)
            .margin_end(24)
            .build();

        let inner = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .build();

        // ── Form hero ─────────────────────────────────────────────────────────
        // TSB is the page's headline — the one number that says what you can
        // absorb today. CTL/ATL are the supporting pair, and the PMC below
        // shows how you got here.
        let hero = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .build();
        hero.append(
            &gtk::Label::builder()
                .label("Form")
                .halign(gtk::Align::Start)
                .css_classes(["caption-heading", "dim-label"])
                .tooltip_text(
                    "Form (TSB) is fitness (CTL) minus fatigue (ATL) — exponential moving \
                     averages of your daily training stress. Positive means fresh, negative \
                     means you are carrying fatigue.",
                )
                .build(),
        );

        let hero_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(18)
            .build();
        let tsb_label = gtk::Label::builder()
            .label("—")
            .css_classes(["display", "numeric"])
            .halign(gtk::Align::Start)
            .build();
        hero_row.append(&tsb_label);

        let hero_text = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .valign(gtk::Align::Center)
            .build();
        let form_phrase = gtk::Label::builder()
            .label("Complete a workout to start tracking form")
            .css_classes(["title-3"])
            .halign(gtk::Align::Start)
            .wrap(true)
            .build();
        let ctl_atl_pair = gtk::Label::builder()
            .label("")
            .css_classes(["caption", "dim-label"])
            .halign(gtk::Align::Start)
            .build();
        hero_text.append(&form_phrase);
        hero_text.append(&ctl_atl_pair);
        hero_row.append(&hero_text);
        hero.append(&hero_row);

        let icu_indicator = gtk::Label::builder()
            .label("")
            .css_classes(["caption", "dim-label"])
            .halign(gtk::Align::Start)
            .visible(false)
            .build();

        // ── Performance Management Chart ──────────────────────────────────────
        // (date, ctl, atl, tsb) series for the past 90 days
        let pmc_data: Rc<RefCell<Vec<PmcPoint>>> = Rc::new(RefCell::new(Vec::new()));

        // The "accent" style class makes widget.color() resolve to the GNOME
        // accent colour (no accent API in libadwaita 1.5); the neutral fg for
        // supporting strokes comes from the parent widget instead.
        let pmc_chart = gtk::DrawingArea::builder()
            .content_height(170)
            .hexpand(true)
            .css_classes(["accent"])
            .accessible_role(gtk::AccessibleRole::Img)
            .build();
        pmc_chart.update_property(&[gtk::accessible::Property::Label(
            "Performance Management Chart: 90-day history of fitness (CTL), fatigue (ATL), and form (TSB)",
        )]);

        {
            let pd = Rc::clone(&pmc_data);
            pmc_chart.set_draw_func(move |widget, cr, width, height| {
                let data = pd.borrow();
                if data.len() < 2 {
                    return;
                }
                // Theme-aware: the headline CTL series draws in the GNOME
                // accent colour (widget carries the "accent" class), while
                // supporting strokes use the parent's neutral foreground.
                let accent = widget.color();
                let (ar, ag, ab) = (
                    accent.red() as f64,
                    accent.green() as f64,
                    accent.blue() as f64,
                );
                let fg = widget.parent().map(|p| p.color()).unwrap_or(accent);
                let (fr, fgr, fb) = (fg.red() as f64, fg.green() as f64, fg.blue() as f64);
                let w = width as f64;
                let h = height as f64;
                let n = data.len();

                let all_vals: Vec<f64> = data.iter().flat_map(|&(_, c, a, s)| [c, a, s]).collect();
                let y_min = all_vals
                    .iter()
                    .cloned()
                    .fold(f64::INFINITY, f64::min)
                    .min(0.0);
                let y_max = all_vals
                    .iter()
                    .cloned()
                    .fold(f64::NEG_INFINITY, f64::max)
                    .max(10.0);
                let y_span = (y_max - y_min).max(1.0);

                // Reserve 16 px at the bottom for x-axis date labels
                let pad_t = 6.0;
                let pad_b = 22.0;
                let usable = h - pad_t - pad_b;

                let x_at = |i: usize| i as f64 / (n - 1).max(1) as f64 * w;
                let y_at = |v: f64| pad_t + (1.0 - (v - y_min) / y_span) * usable;

                // Zero line (thin, dimmed)
                let zero_y = y_at(0.0);
                cr.set_source_rgba(fr, fgr, fb, 0.25);
                cr.set_line_width(1.0);
                cr.move_to(0.0, zero_y);
                cr.line_to(w, zero_y);
                cr.stroke().ok();

                // TSB as a soft fill against the zero line — no third line needed
                {
                    cr.new_path();
                    cr.move_to(x_at(0), zero_y);
                    for (i, &(_, _, _, s)) in data.iter().enumerate() {
                        cr.line_to(x_at(i), y_at(s));
                    }
                    cr.line_to(x_at(n - 1), zero_y);
                    cr.close_path();
                    cr.set_source_rgba(fr, fgr, fb, 0.08);
                    cr.fill().ok();
                }

                // Draw a single series as a line — field_idx: 1=CTL, 2=ATL
                let draw_series = |field_idx: usize| {
                    let vals: Vec<f64> = data
                        .iter()
                        .map(|&(_, c, a, _)| if field_idx == 1 { c } else { a })
                        .collect();
                    cr.move_to(x_at(0), y_at(vals[0]));
                    for (i, &v) in vals.iter().enumerate().skip(1) {
                        cr.line_to(x_at(i), y_at(v));
                    }
                    cr.stroke().ok();
                };

                // ATL: thin dashed — draw first so CTL renders on top
                cr.set_source_rgba(fr, fgr, fb, 0.45);
                cr.set_line_width(1.5);
                cr.set_dash(&[4.0, 3.0], 0.0);
                draw_series(2);
                cr.set_dash(&[], 0.0);

                // CTL: the bold headline series, in the accent colour
                cr.set_source_rgba(ar, ag, ab, 0.90);
                cr.set_line_width(2.5);
                draw_series(1);

                // Mark today on the CTL line
                if let Some(&(_, ctl_last, _, _)) = data.last() {
                    cr.arc(x_at(n - 1), y_at(ctl_last), 3.5, 0.0, std::f64::consts::TAU);
                    cr.fill().ok();
                }

                // X-axis: draw a tick and short month label at the 1st of each month
                cr.set_source_rgba(fr, fgr, fb, 0.55);
                cr.set_font_size(10.0);
                let axis_y = h - pad_b + 4.0;
                let label_y = h - 4.0;
                for (i, &(date, _, _, _)) in data.iter().enumerate() {
                    if date.day() == 1 {
                        let x = x_at(i);
                        cr.set_line_width(1.0);
                        cr.move_to(x, axis_y - 4.0);
                        cr.line_to(x, axis_y);
                        cr.stroke().ok();
                        let label = date.format("%b").to_string();
                        cr.move_to(x + 2.0, label_y);
                        cr.show_text(&label).ok();
                    }
                }
            });
        }

        // Legend mirrors the drawing: CTL is genuinely accent-coloured, the
        // rest are neutral line styles.
        let pmc_legend = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(18)
            .build();
        for (label, css) in [
            ("━ Fitness (CTL)", "accent"),
            ("╌ Fatigue (ATL)", "dim-label"),
            ("▒ Form (TSB)", "dim-label"),
        ] {
            pmc_legend.append(
                &gtk::Label::builder()
                    .label(label)
                    .css_classes(["caption", css])
                    .build(),
            );
        }

        let pmc_section = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .visible(false)
            .build();
        pmc_section.append(&pmc_chart);
        pmc_section.append(&pmc_legend);
        pmc_section.append(&icu_indicator);
        hero.append(&pmc_section);
        inner.append(&hero);

        // ── Wellness ──────────────────────────────────────────────────────────
        inner.append(
            &gtk::Label::builder()
                .label("Wellness")
                .halign(gtk::Align::Start)
                .css_classes(["heading"])
                .tooltip_text(
                    "HRV, resting heart rate, sleep, and activity data synced from \
                     Intervals.icu (Preferences → Intervals.icu)",
                )
                .build(),
        );

        // 6 sparkline cards in a 2-column responsive FlowBox
        let wellness_flow = gtk::FlowBox::builder()
            .column_spacing(12)
            .row_spacing(12)
            .max_children_per_line(2)
            .min_children_per_line(1)
            .selection_mode(gtk::SelectionMode::None)
            .homogeneous(true)
            .build();

        let (hrv_card, hrv_value, hrv_trend, hrv_chart, hrv_data) =
            Self::make_wellness_card("HRV", "");
        let (rhr_card, rhr_value, rhr_trend, rhr_chart, rhr_data) =
            Self::make_wellness_card("Resting Heart Rate", "bpm");
        let (sleep_card, sleep_value, sleep_trend, sleep_chart, sleep_data) =
            Self::make_wellness_card("Sleep", "hours");
        let (score_card, score_value, score_trend, score_chart, score_data) =
            Self::make_wellness_card("Sleep Score", "/ 100");
        let (steps_card, steps_value, steps_trend, steps_chart, steps_data) =
            Self::make_wellness_card("Steps", "today");
        let (cal_card, cal_value, cal_trend, cal_chart, cal_data) =
            Self::make_wellness_card("Calories", "kcal");

        for card in [
            &hrv_card,
            &rhr_card,
            &sleep_card,
            &score_card,
            &steps_card,
            &cal_card,
        ] {
            wellness_flow.append(card);
        }
        for i in 0..6i32 {
            if let Some(child) = wellness_flow.child_at_index(i) {
                child.set_hexpand(true);
            }
        }

        let wellness_no_data = gtk::Label::builder()
            .label(
                "No wellness data yet — sync Intervals.icu in Preferences to see \
                 HRV, sleep, and step data from your connected devices.",
            )
            .css_classes(["dim-label"])
            .halign(gtk::Align::Start)
            .wrap(true)
            .visible(false)
            .build();

        inner.append(&wellness_no_data);
        inner.append(&wellness_flow);

        // ── Load history (weekly TSS bars + volume strip) ─────────────────────
        // Built here, appended to `inner` after the zones/bests sections so the
        // page reads: form → wellness → zones → bests → load → coach.
        let load_section = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .build();
        load_section.append(
            &gtk::Label::builder()
                .label("Load History")
                .halign(gtk::Align::Start)
                .css_classes(["heading"])
                .tooltip_text(
                    "Training Stress Score per week for the past 6 weeks — higher bars \
                     mean harder weeks. A sustainable build is roughly 5–10% per week.",
                )
                .build(),
        );

        let tss_week_data: Rc<RefCell<Vec<f32>>> = Rc::new(RefCell::new(vec![0.0; 6]));

        // "accent" class → widget.color() resolves to the GNOME accent colour
        // (see the PMC chart note)
        let tss_chart = gtk::DrawingArea::builder()
            .content_height(120)
            .hexpand(true)
            .css_classes(["accent"])
            .accessible_role(gtk::AccessibleRole::Img)
            .build();
        tss_chart.update_property(&[gtk::accessible::Property::Label(
            "Weekly TSS bar chart: training stress score for the past 6 weeks",
        )]);

        let tss_data_ref = Rc::clone(&tss_week_data);
        tss_chart.set_draw_func(move |widget, cr, width, height| {
            let weeks = tss_data_ref.borrow();
            let max_tss = weeks.iter().copied().fold(0.0f32, f32::max);
            let fg = widget.color();
            let (fr, fgr, fb) = (fg.red() as f64, fg.green() as f64, fg.blue() as f64);
            let w = width as f64;
            let h = height as f64;
            let n = weeks.len() as f64;
            let gap = 6.0;
            let bar_w = ((w - gap * (n - 1.0)) / n).max(1.0);

            for (i, &tss) in weeks.iter().enumerate() {
                let x = i as f64 * (bar_w + gap);
                cr.set_source_rgba(fr, fgr, fb, 0.10);
                cr.rectangle(x, 0.0, bar_w, h);
                cr.fill().ok();

                if max_tss > 0.0 && tss > 0.0 {
                    let bar_h = (tss as f64 / max_tss as f64 * h).max(2.0);
                    cr.set_source_rgba(fr, fgr, fb, 0.65);
                    cr.rectangle(x, h - bar_h, bar_w, bar_h);
                    cr.fill().ok();
                }
            }
        });

        load_section.append(&tss_chart);

        let week_label_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .homogeneous(true)
            .build();
        let mut week_header_labels: Vec<gtk::Label> = Vec::with_capacity(6);
        // Two rows: week number + TSS value
        let tss_value_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .homogeneous(true)
            .build();
        let mut tss_value_labels: Vec<gtk::Label> = Vec::with_capacity(6);
        for _ in 0..6 {
            let lbl = gtk::Label::builder()
                .label("")
                .css_classes(["caption", "dim-label"])
                .halign(gtk::Align::Center)
                .build();
            week_label_row.append(&lbl);
            week_header_labels.push(lbl);

            let vlbl = gtk::Label::builder()
                .label("")
                .css_classes(["caption", "numeric"])
                .halign(gtk::Align::Center)
                .build();
            tss_value_row.append(&vlbl);
            tss_value_labels.push(vlbl);
        }
        load_section.append(&week_label_row);
        load_section.append(&tss_value_row);

        // ── Power Curve ───────────────────────────────────────────────────────
        const CURVE_DURATIONS: [usize; 10] = [5, 10, 30, 60, 120, 300, 600, 1200, 1800, 3600];
        const CURVE_LABELS: [&str; 10] = [
            "5s", "10s", "30s", "1m", "2m", "5m", "10m", "20m", "30m", "60m",
        ];

        // Shared data: (all-time peak, 30-day peak) per duration
        let curve_data: Rc<RefCell<Vec<(u32, u32)>>> =
            Rc::new(RefCell::new(vec![(0, 0); CURVE_DURATIONS.len()]));

        let curve_chart = gtk::DrawingArea::builder()
            .content_height(130)
            .hexpand(true)
            .accessible_role(gtk::AccessibleRole::Img)
            .build();
        curve_chart.update_property(&[gtk::accessible::Property::Label(
            "Power curve chart: best mean maximal power for durations from 1 second to 60 minutes",
        )]);

        let curve_data_draw = Rc::clone(&curve_data);
        let ftp_for_curve = Rc::clone(&ftp);
        curve_chart.set_draw_func(move |widget, cr, width, height| {
            let data = curve_data_draw.borrow();
            let max_w = data.iter().map(|&(a, _)| a).max().unwrap_or(0);
            if max_w == 0 {
                return;
            }
            let ftp_val = ftp_for_curve.get();
            let fg = widget.color();
            let (fr, fgr, fb) = (fg.red() as f64, fg.green() as f64, fg.blue() as f64);
            let w = width as f64;
            let h = height as f64;
            let n = data.len();
            let pad_b = 4.0f64;
            let usable_h = h - pad_b;
            let max_f = max_w as f64;

            let x_at = |i: usize| (i as f64 / (n - 1).max(1) as f64) * w;
            let y_at = |p: u32| {
                if p == 0 {
                    h
                } else {
                    usable_h - (p as f64 / max_f) * (usable_h - 4.0)
                }
            };

            // 30-day curve first (dimmed fg), so the all-time zone dots sit on top
            let mo_pts: Vec<(f64, f64)> = data
                .iter()
                .enumerate()
                .filter(|(_, &(_, m))| m > 0)
                .map(|(i, &(_, m))| (x_at(i), y_at(m)))
                .collect();
            if mo_pts.len() >= 2 {
                cr.set_source_rgba(fr, fgr, fb, 0.40);
                cr.set_line_width(1.5);
                cr.move_to(mo_pts[0].0, mo_pts[0].1);
                for &(x, y) in &mo_pts[1..] {
                    cr.line_to(x, y);
                }
                cr.stroke().ok();
                for &(x, y) in &mo_pts {
                    cr.arc(x, y, 2.5, 0.0, std::f64::consts::TAU);
                    cr.fill().ok();
                }
            }

            // All-time curve: a quiet fg line carrying dots coloured by the
            // power zone each best falls in — the sprint end glows anaerobic
            // red, the hour end sits at threshold. Zone RGB is the app's only
            // sanctioned expressive colour (CLAUDE.md §1.6).
            let at_pts: Vec<(usize, f64, f64)> = data
                .iter()
                .enumerate()
                .filter(|(_, &(a, _))| a > 0)
                .map(|(i, &(a, _))| (i, x_at(i), y_at(a)))
                .collect();
            if at_pts.len() >= 2 {
                cr.set_source_rgba(fr, fgr, fb, 0.30);
                cr.set_line_width(1.5);
                cr.move_to(at_pts[0].1, at_pts[0].2);
                for &(_, x, y) in &at_pts[1..] {
                    cr.line_to(x, y);
                }
                cr.stroke().ok();
                for &(i, x, y) in &at_pts {
                    let watts = data[i].0;
                    let (zr, zg, zb) = ZONE_COLORS[power_zone_index(watts, ftp_val)];
                    cr.set_source_rgba(zr, zg, zb, 1.0);
                    cr.arc(x, y, 4.0, 0.0, std::f64::consts::TAU);
                    cr.fill().ok();
                }
            }
        });

        // X-axis duration labels
        let curve_x_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(0)
            .homogeneous(true)
            .build();
        for lbl in CURVE_LABELS {
            curve_x_row.append(
                &gtk::Label::builder()
                    .label(lbl)
                    .css_classes(["caption", "dim-label"])
                    .halign(gtk::Align::Center)
                    .build(),
            );
        }

        // Peak watt labels per duration (updated in reload)
        let curve_w_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(0)
            .homogeneous(true)
            .build();
        let mut curve_w_labels: Vec<gtk::Label> = Vec::with_capacity(CURVE_DURATIONS.len());
        for _ in &CURVE_DURATIONS {
            let lbl = gtk::Label::builder()
                .label("—")
                .css_classes(["caption", "numeric"])
                .halign(gtk::Align::Center)
                .build();
            curve_w_row.append(&lbl);
            curve_w_labels.push(lbl);
        }

        // Legend
        let curve_legend = gtk::Label::builder()
            .label("Dots coloured by power zone · dimmed line = last 30 days")
            .css_classes(["caption", "dim-label"])
            .halign(gtk::Align::Start)
            .build();

        let curve_section = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .visible(false)
            .build();
        curve_section.append(
            &gtk::Label::builder()
                .label("Peak power")
                .halign(gtk::Align::Start)
                .css_classes(["caption-heading", "dim-label"])
                .tooltip_text(
                    "Best average power for each duration across all recorded sessions, \
                     coloured by the power zone it falls in at your current FTP",
                )
                .build(),
        );
        curve_section.append(&curve_chart);
        curve_section.append(&curve_x_row);
        curve_section.append(&curve_w_row);
        curve_section.append(&curve_legend);

        // ── Pace Curve (running) ──────────────────────────────────────────────
        let pace_data: Rc<RefCell<Vec<(u32, u32)>>> =
            Rc::new(RefCell::new(vec![(0, 0); PACE_DISTANCES.len()]));

        let pace_chart = gtk::DrawingArea::builder()
            .content_height(130)
            .hexpand(true)
            .accessible_role(gtk::AccessibleRole::Img)
            .build();
        pace_chart.update_property(&[gtk::accessible::Property::Label(
            "Pace curve chart: best times at standard running distances (400 m to marathon)",
        )]);
        {
            let pd = Rc::clone(&pace_data);
            pace_chart.set_draw_func(move |widget, cr, width, height| {
                let fg = widget.color();
                let (fr, fgr, fb) = (fg.red() as f64, fg.green() as f64, fg.blue() as f64);
                let data = pd.borrow();
                // Scale from both curves so neither goes out of range.
                let min_p = data
                    .iter()
                    .flat_map(|&(a, m)| [a, m])
                    .filter(|&p| p > 0)
                    .min()
                    .unwrap_or(0);
                let max_p = data
                    .iter()
                    .flat_map(|&(a, m)| [a, m])
                    .filter(|&p| p > 0)
                    .max()
                    .unwrap_or(0);
                if min_p == 0 && max_p == 0 {
                    return;
                }
                let w = width as f64;
                let h = height as f64;
                let n = data.len();
                let p_range = (max_p as f64 - min_p as f64).max(30.0);
                let usable_h = h - 4.0;

                let x_at = |i: usize| (i as f64 / (n - 1).max(1) as f64) * w;
                // Faster (lower sec/km) → top; slower → bottom.
                let y_at = |pace: u32| -> f64 {
                    let ratio = ((pace as f64 - min_p as f64) / p_range).clamp(0.0, 1.0);
                    4.0 + ratio * (usable_h - 4.0)
                };

                // Theme-aware: solid fg = all-time, dimmed fg = last 30 days
                let draw_curve = |pts: &[(f64, f64)], alpha: f64, radius: f64| {
                    if pts.len() < 2 {
                        return;
                    }
                    cr.set_source_rgba(fr, fgr, fb, alpha);
                    cr.set_line_width(2.0);
                    cr.move_to(pts[0].0, pts[0].1);
                    for &(x, y) in &pts[1..] {
                        cr.line_to(x, y);
                    }
                    cr.stroke().ok();
                    for &(x, y) in pts {
                        cr.arc(x, y, radius, 0.0, std::f64::consts::TAU);
                        cr.fill().ok();
                    }
                };

                let mo_pts: Vec<(f64, f64)> = data
                    .iter()
                    .enumerate()
                    .filter(|(_, &(_, m))| m > 0)
                    .map(|(i, &(_, m))| (x_at(i), y_at(m)))
                    .collect();
                draw_curve(&mo_pts, 0.40, 2.5);

                let at_pts: Vec<(f64, f64)> = data
                    .iter()
                    .enumerate()
                    .filter(|(_, &(a, _))| a > 0)
                    .map(|(i, &(a, _))| (x_at(i), y_at(a)))
                    .collect();
                draw_curve(&at_pts, 0.85, 3.5);
            });
        }

        let pace_x_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(0)
            .homogeneous(true)
            .build();
        for lbl in PACE_LABELS {
            pace_x_row.append(
                &gtk::Label::builder()
                    .label(lbl)
                    .css_classes(["caption", "dim-label"])
                    .halign(gtk::Align::Center)
                    .build(),
            );
        }

        let pace_val_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(0)
            .homogeneous(true)
            .build();
        let mut pace_val_labels: Vec<gtk::Label> = Vec::with_capacity(PACE_DISTANCES.len());
        for _ in &PACE_DISTANCES {
            let lbl = gtk::Label::builder()
                .label("—")
                .css_classes(["caption", "numeric"])
                .halign(gtk::Align::Center)
                .build();
            pace_val_row.append(&lbl);
            pace_val_labels.push(lbl);
        }

        let pace_legend = gtk::Label::builder()
            .label("Solid = all time · dimmed = last 30 days")
            .css_classes(["caption", "dim-label"])
            .halign(gtk::Align::Start)
            .build();

        let pace_section = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .visible(false)
            .build();
        pace_section.append(
            &gtk::Label::builder()
                .label("Running pace")
                .halign(gtk::Align::Start)
                .css_classes(["caption-heading", "dim-label"])
                .tooltip_text("Best pace for each distance across all synced running activities")
                .build(),
        );
        pace_section.append(&pace_chart);
        pace_section.append(&pace_x_row);
        pace_section.append(&pace_val_row);
        pace_section.append(&pace_legend);

        // ── Volume strip (inside Load History) ────────────────────────────────
        let volume_section = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(24)
            .margin_top(6)
            .visible(false)
            .tooltip_text(
                "Kilojoules measure total mechanical work done — a more accurate proxy \
                 for training load than time alone",
            )
            .build();
        let make_stat = |caption: &str| -> (gtk::Box, gtk::Label) {
            let col = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .build();
            col.append(
                &gtk::Label::builder()
                    .label(caption)
                    .halign(gtk::Align::Start)
                    .css_classes(["caption", "dim-label"])
                    .build(),
            );
            let value = gtk::Label::builder()
                .label("—")
                .halign(gtk::Align::Start)
                .css_classes(["title-4", "numeric"])
                .build();
            col.append(&value);
            (col, value)
        };
        let (wkj_col, week_kj_label) = make_stat("This week");
        let (whrs_col, week_hrs_label) = make_stat("Time this week");
        let (mkj_col, month_kj_label) = make_stat("This month");
        let (tot_col, total_sessions_label) = make_stat("Sessions");
        volume_section.append(&wkj_col);
        volume_section.append(&whrs_col);
        volume_section.append(&mkj_col);
        volume_section.append(&tot_col);
        load_section.append(&volume_section);

        // ── Zone Distribution ─────────────────────────────────────────────────
        let zone_seconds: Rc<RefCell<[u32; 7]>> = Rc::new(RefCell::new([0u32; 7]));

        let zone_bar = gtk::DrawingArea::builder()
            .content_height(20)
            .hexpand(true)
            .accessible_role(gtk::AccessibleRole::Img)
            .build();
        zone_bar.update_property(&[gtk::accessible::Property::Label(
            "Power zone distribution bar: proportional time in zones Z1 through Z7",
        )]);

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
            .spacing(6)
            .visible(false)
            .build();
        zone_section.append(
            &gtk::Label::builder()
                .label("Power")
                .halign(gtk::Align::Start)
                .css_classes(["caption-heading", "dim-label"])
                .tooltip_text(
                    "Time spent in each power zone, all recorded sessions. Endurance \
                     athletes typically aim for 70–80% in Z1–Z2 (polarised) or Z2–Z3 \
                     (pyramidal)",
                )
                .build(),
        );
        zone_section.append(&zone_bar);
        zone_section.append(&zone_legend);

        // ── Heart Rate Zones ──────────────────────────────────────────────────
        let hr_zone_seconds: Rc<RefCell<[u32; 5]>> = Rc::new(RefCell::new([0u32; 5]));

        let hr_zone_bar = gtk::DrawingArea::builder()
            .content_height(20)
            .hexpand(true)
            .accessible_role(gtk::AccessibleRole::Img)
            .build();
        hr_zone_bar.update_property(&[gtk::accessible::Property::Label(
            "Heart rate zone distribution bar: proportional time in HR zones Z1 through Z5",
        )]);
        {
            let hz_ref = Rc::clone(&hr_zone_seconds);
            hr_zone_bar.set_draw_func(move |_widget, cr, width, height| {
                let zones = hz_ref.borrow();
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
                    // HR zones share the power-zone ramp (Z1–Z5) — one palette
                    // across the whole app, per the zone-colour design language.
                    let (r, g, b) = ZONE_COLORS[i];
                    cr.set_source_rgba(r, g, b, 0.85);
                    cr.rectangle(x, 0.0, seg_w, h);
                    cr.fill().ok();
                    x += seg_w;
                }
            });
        }

        let hr_zone_legend = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(0)
            .homogeneous(true)
            .build();
        for label in [
            "Z1 Easy",
            "Z2 Aerobic",
            "Z3 Tempo",
            "Z4 Threshold",
            "Z5 Max",
        ] {
            hr_zone_legend.append(
                &gtk::Label::builder()
                    .label(label)
                    .css_classes(["caption", "dim-label"])
                    .halign(gtk::Align::Center)
                    .build(),
            );
        }

        let hr_zone_section = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .visible(false)
            .build();
        hr_zone_section.append(
            &gtk::Label::builder()
                .label("Heart rate")
                .halign(gtk::Align::Start)
                .css_classes(["caption-heading", "dim-label"])
                .tooltip_text(
                    "Time in each HR zone based on your recorded max HR — in-app \
                     sessions only",
                )
                .build(),
        );
        hr_zone_section.append(&hr_zone_bar);
        hr_zone_section.append(&hr_zone_legend);

        // ── Section wrappers and page order ───────────────────────────────────
        let zones_section = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .visible(false)
            .build();
        zones_section.append(
            &gtk::Label::builder()
                .label("Where the Time Goes")
                .halign(gtk::Align::Start)
                .css_classes(["heading"])
                .build(),
        );
        zones_section.append(&zone_section);
        zones_section.append(&hr_zone_section);
        inner.append(&zones_section);

        let bests_section = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .visible(false)
            .build();
        bests_section.append(
            &gtk::Label::builder()
                .label("Bests")
                .halign(gtk::Align::Start)
                .css_classes(["heading"])
                .build(),
        );
        bests_section.append(&curve_section);
        bests_section.append(&pace_section);
        inner.append(&bests_section);

        inner.append(&load_section);

        // ── Coach — one card for all AI output ────────────────────────────────
        // Fitness analysis on top, retrospectives below behind a Week|Month
        // switcher (same linked-toggle pattern as the calendar).
        let coach_card = gtk::Box::builder()
            .css_classes(["card"])
            .orientation(gtk::Orientation::Vertical)
            .build();

        let ai_header = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .margin_top(12)
            .margin_bottom(6)
            .margin_start(12)
            .margin_end(12)
            .build();
        ai_header.append(
            &gtk::Image::builder()
                .icon_name("chat-message-new-symbolic")
                .css_classes(["dim-label"])
                .build(),
        );
        ai_header.append(
            &gtk::Label::builder()
                .label("Coach")
                .css_classes(["heading"])
                .halign(gtk::Align::Start)
                .hexpand(true)
                .tooltip_text(
                    "AI interpretation of your training metrics, recovery signals, \
                     and wellness data",
                )
                .build(),
        );
        let analyse_spinner = gtk::Spinner::new();
        analyse_spinner.set_visible(false);
        ai_header.append(&analyse_spinner);

        let analyse_btn = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .css_classes(["flat", "circular"])
            .tooltip_text("Refresh AI fitness analysis")
            .valign(gtk::Align::Center)
            .build();
        ai_header.append(&analyse_btn);
        coach_card.append(&ai_header);
        coach_card.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

        // Container that holds either structured sections or a single fallback label
        let ai_content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(0)
            .build();

        let analyse_label = gtk::Label::builder()
            .label(
                "Select the refresh button above to get an AI-powered interpretation \
                 of your training metrics, recovery signals, and wellness data.",
            )
            .css_classes(["dim-label"])
            .halign(gtk::Align::Start)
            .valign(gtk::Align::Start)
            .wrap(true)
            .selectable(true)
            .xalign(0.0)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        ai_content.append(&analyse_label);
        coach_card.append(&ai_content);
        coach_card.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

        let retro_header = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(12)
            .margin_end(12)
            .build();
        retro_header.append(
            &gtk::Label::builder()
                .label("Retrospective")
                .css_classes(["heading"])
                .halign(gtk::Align::Start)
                .hexpand(true)
                .tooltip_text(
                    "AI analysis of strain, recovery patterns, and performance trends \
                     over the selected period",
                )
                .build(),
        );
        let retro_spinner = gtk::Spinner::new();
        retro_spinner.set_visible(false);
        retro_header.append(&retro_spinner);

        let week_toggle = gtk::ToggleButton::builder()
            .label("Week")
            .active(true)
            .build();
        let month_toggle = gtk::ToggleButton::builder().label("Month").build();
        month_toggle.set_group(Some(&week_toggle));
        let toggle_box = gtk::Box::builder()
            .css_classes(["linked"])
            .valign(gtk::Align::Center)
            .build();
        toggle_box.append(&week_toggle);
        toggle_box.append(&month_toggle);
        retro_header.append(&toggle_box);

        let generate_btn = gtk::Button::builder()
            .label("Generate")
            .css_classes(["pill"])
            .tooltip_text("Generate an AI retrospective for the selected period")
            .valign(gtk::Align::Center)
            .build();
        retro_header.append(&generate_btn);
        coach_card.append(&retro_header);

        let (weekly_content, weekly_label) = build_retro_content("past 7 days");
        let (monthly_content, monthly_label) = build_retro_content("past 30 days");

        let retro_stack = gtk::Stack::builder()
            .vhomogeneous(false)
            .transition_type(gtk::StackTransitionType::Crossfade)
            .build();
        retro_stack.add_named(&weekly_content, Some("week"));
        retro_stack.add_named(&monthly_content, Some("month"));
        coach_card.append(&retro_stack);
        {
            let stack = retro_stack.clone();
            week_toggle.connect_toggled(move |t| {
                if t.is_active() {
                    stack.set_visible_child_name("week");
                }
            });
            let stack = retro_stack.clone();
            month_toggle.connect_toggled(move |t| {
                if t.is_active() {
                    stack.set_visible_child_name("month");
                }
            });
        }
        inner.append(&coach_card);

        clamp.set_child(Some(&inner));
        scroll.set_child(Some(&clamp));
        root.append(&scroll);

        // ── Reload closure ────────────────────────────────────────────────────
        let ftp_for_reload = Rc::clone(&ftp);
        let reload: Rc<dyn Fn()> = {
            let pool = pool.clone();
            let rt_handle = rt_handle.clone();
            let tss_week_data = Rc::clone(&tss_week_data);
            let ftp = ftp_for_reload;
            let analyse_label_r = analyse_label.clone();
            let ai_content_r = ai_content.clone();
            let icu_indicator_r = icu_indicator.clone();
            let curve_data_r = Rc::clone(&curve_data);
            let curve_w_labels_r = curve_w_labels.clone();
            let curve_section_r = curve_section.clone();
            let curve_chart_r = curve_chart.clone();
            let pace_data_r = Rc::clone(&pace_data);
            let pace_section_r = pace_section.clone();
            let pace_chart_r = pace_chart.clone();
            let pace_val_labels_r = pace_val_labels.clone();
            let hr_zone_seconds_r = Rc::clone(&hr_zone_seconds);
            let hr_zone_bar_r = hr_zone_bar.clone();
            let hr_zone_section_r = hr_zone_section.clone();
            let athlete_r = Rc::clone(&athlete);
            let pmc_data_r = Rc::clone(&pmc_data);
            let pmc_section_r = pmc_section.clone();
            let pmc_chart_r = pmc_chart.clone();
            let volume_section_r = volume_section.clone();
            let api_banner_r = api_banner.clone();
            Rc::new(move || {
                // API key pre-flight check (local keyring — fast, stays synchronous)
                let has_api_key = keystore::get_secret(keystore::KEY_ANTHROPIC)
                    .unwrap_or(None)
                    .map(|k| !k.trim().is_empty())
                    .unwrap_or(false);
                api_banner_r.set_revealed(!has_api_key);

                // Load every data source off the main thread (CLAUDE.md §2.3), then
                // update the charts and cards once the data arrives. Clone the widget
                // handles the callback needs (cheap refcount bumps).
                let pool_load = pool.clone();
                let icu_indicator_r = icu_indicator_r.clone();
                let ftp = Rc::clone(&ftp);
                let pmc_data_r = Rc::clone(&pmc_data_r);
                let pmc_section_r = pmc_section_r.clone();
                let pmc_chart_r = pmc_chart_r.clone();
                let tsb_label = tsb_label.clone();
                let form_phrase = form_phrase.clone();
                let ctl_atl_pair = ctl_atl_pair.clone();
                let zones_section_r = zones_section.clone();
                let bests_section_r = bests_section.clone();
                let wellness_no_data = wellness_no_data.clone();
                let wellness_flow = wellness_flow.clone();
                let hrv_value = hrv_value.clone();
                let hrv_trend = hrv_trend.clone();
                let hrv_data = Rc::clone(&hrv_data);
                let hrv_chart = hrv_chart.clone();
                let rhr_value = rhr_value.clone();
                let rhr_trend = rhr_trend.clone();
                let rhr_data = Rc::clone(&rhr_data);
                let rhr_chart = rhr_chart.clone();
                let sleep_value = sleep_value.clone();
                let sleep_trend = sleep_trend.clone();
                let sleep_data = Rc::clone(&sleep_data);
                let sleep_chart = sleep_chart.clone();
                let score_value = score_value.clone();
                let score_trend = score_trend.clone();
                let score_data = Rc::clone(&score_data);
                let score_chart = score_chart.clone();
                let steps_value = steps_value.clone();
                let steps_trend = steps_trend.clone();
                let steps_data = Rc::clone(&steps_data);
                let steps_chart = steps_chart.clone();
                let cal_value = cal_value.clone();
                let cal_trend = cal_trend.clone();
                let cal_data = Rc::clone(&cal_data);
                let cal_chart = cal_chart.clone();
                let volume_section_r = volume_section_r.clone();
                let week_kj_label = week_kj_label.clone();
                let week_hrs_label = week_hrs_label.clone();
                let month_kj_label = month_kj_label.clone();
                let total_sessions_label = total_sessions_label.clone();
                let week_header_labels = week_header_labels.clone();
                let tss_value_labels = tss_value_labels.clone();
                let tss_week_data = Rc::clone(&tss_week_data);
                let tss_chart = tss_chart.clone();
                let zone_seconds = Rc::clone(&zone_seconds);
                let zone_bar = zone_bar.clone();
                let zone_section = zone_section.clone();
                let curve_data_r = Rc::clone(&curve_data_r);
                let curve_section_r = curve_section_r.clone();
                let curve_chart_r = curve_chart_r.clone();
                let curve_w_labels_r = curve_w_labels_r.clone();
                let pace_data_r = Rc::clone(&pace_data_r);
                let pace_section_r = pace_section_r.clone();
                let pace_chart_r = pace_chart_r.clone();
                let pace_val_labels_r = pace_val_labels_r.clone();
                let hr_zone_seconds_r = Rc::clone(&hr_zone_seconds_r);
                let hr_zone_bar_r = hr_zone_bar_r.clone();
                let hr_zone_section_r = hr_zone_section_r.clone();
                let athlete_r = Rc::clone(&athlete_r);
                let ai_content_r = ai_content_r.clone();
                let analyse_label_r = analyse_label_r.clone();

                crate::ui::spawn_to_main(
                    &rt_handle,
                    async move {
                        let records = db::load_session_records(&pool_load)
                            .await
                            .unwrap_or_default();
                        let intervals_pairs = db::load_intervals_tss_pairs(&pool_load)
                            .await
                            .unwrap_or_default();
                        let icu_activities = db::load_unlinked_intervals_activities(&pool_load)
                            .await
                            .unwrap_or_default();
                        let wellness = db::load_wellness_recent(&pool_load, 14)
                            .await
                            .unwrap_or_default();
                        let run_streams = db::load_run_activity_streams(&pool_load)
                            .await
                            .unwrap_or_default();
                        let cached_insight = db::get_setting(&pool_load, "ai.fitness_insight")
                            .await
                            .unwrap_or(None)
                            .unwrap_or_default();
                        (
                            records,
                            intervals_pairs,
                            icu_activities,
                            wellness,
                            run_streams,
                            cached_insight,
                        )
                    },
                    move |(
                        records,
                        intervals_pairs,
                        icu_activities,
                        wellness,
                        run_streams,
                        cached_insight,
                    )| {
                        if !icu_activities.is_empty() {
                            icu_indicator_r.set_label(&format!(
                                "Includes {} activities synced from Intervals.icu",
                                icu_activities.len()
                            ));
                            icu_indicator_r.set_visible(true);
                        } else {
                            icu_indicator_r.set_visible(false);
                        }

                        let ftp_val = ftp.get();
                        let today = Local::now().date_naive();

                        // This page needs the samples themselves for the zone
                        // distribution and the power curve, so it loads full
                        // records and reduces them here rather than querying twice.
                        let rides: Vec<db::SessionSummary> =
                            records.iter().map(|r| r.summary()).collect();

                        let (ctl, atl, _) =
                            compute_load_metrics(&rides, &intervals_pairs, ftp_val, today);
                        let tsb = ctl - atl;

                        // PMC 90-day series
                        let pmc_series =
                            compute_pmc_series(&rides, &intervals_pairs, ftp_val, today);
                        let has_pmc = pmc_series.len() >= 2;
                        pmc_section_r.set_visible(has_pmc);
                        if has_pmc {
                            *pmc_data_r.borrow_mut() = pmc_series;
                            pmc_chart_r.queue_draw();
                        }

                        // Form hero
                        let has_load = ctl > 0.5 || atl > 0.5;
                        if has_load {
                            tsb_label.set_label(&format!("{:+.0}", tsb));
                            form_phrase.set_label(tsb_status_text(tsb));
                            ctl_atl_pair
                                .set_label(&format!("Fitness {:.0} · Fatigue {:.0}", ctl, atl));
                        } else {
                            tsb_label.set_label("—");
                            form_phrase.set_label("Complete a workout to start tracking form");
                            ctl_atl_pair.set_label("");
                        }

                        // TSB value colour — genuinely semantic: fresh is good,
                        // deep fatigue warrants attention
                        tsb_label.remove_css_class("success");
                        tsb_label.remove_css_class("warning");
                        if has_load && tsb > 5.0 {
                            tsb_label.add_css_class("success");
                        } else if has_load && tsb < -10.0 {
                            tsb_label.add_css_class("warning");
                        }

                        // ── Wellness cards ────────────────────────────────────────────
                        let has_wellness = wellness.iter().any(|w| {
                            w.hrv.is_some()
                                || w.resting_hr.is_some()
                                || w.sleep_secs.is_some()
                                || w.steps.is_some()
                                || w.calories.is_some()
                        });
                        wellness_no_data.set_visible(!has_wellness);
                        wellness_flow.set_visible(has_wellness);

                        if has_wellness {
                            let hrv_series = build_14day_series(&wellness, today, |e| e.hrv);
                            let rhr_series = build_14day_series(&wellness, today, |e| {
                                e.resting_hr.map(|v| v as f32)
                            });
                            let sleep_series = build_14day_series(&wellness, today, |e| {
                                e.sleep_secs.map(|s| s as f32 / 3600.0)
                            });
                            let score_series = build_14day_series(&wellness, today, |e| {
                                e.sleep_score.map(|v| v as f32)
                            });
                            let steps_series =
                                build_14day_series(&wellness, today, |e| e.steps.map(|v| v as f32));
                            let cal_series = build_14day_series(&wellness, today, |e| {
                                e.calories.map(|v| v as f32)
                            });

                            update_wellness_card(
                                &hrv_value,
                                &hrv_trend,
                                &hrv_data,
                                &hrv_chart,
                                &hrv_series,
                                "{:.0}",
                                true,
                            );
                            update_wellness_card(
                                &rhr_value,
                                &rhr_trend,
                                &rhr_data,
                                &rhr_chart,
                                &rhr_series,
                                "{:.0}",
                                false,
                            );
                            update_wellness_card(
                                &sleep_value,
                                &sleep_trend,
                                &sleep_data,
                                &sleep_chart,
                                &sleep_series,
                                "{:.1}",
                                true,
                            );
                            update_wellness_card(
                                &score_value,
                                &score_trend,
                                &score_data,
                                &score_chart,
                                &score_series,
                                "{:.0}",
                                true,
                            );
                            update_wellness_card(
                                &steps_value,
                                &steps_trend,
                                &steps_data,
                                &steps_chart,
                                &steps_series,
                                "{:.0}",
                                true,
                            );
                            update_wellness_card(
                                &cal_value,
                                &cal_trend,
                                &cal_data,
                                &cal_chart,
                                &cal_series,
                                "{:.0}",
                                true,
                            );
                        }

                        // Volume
                        let week_start =
                            today - Duration::days(today.weekday().num_days_from_monday() as i64);
                        let month_start = NaiveDate::from_ymd_opt(today.year(), today.month(), 1)
                            .expect("day 1 of any calendar month is always valid");

                        let mut wk_kj = 0.0f32;
                        let mut wk_secs = 0u64;
                        let mut mo_kj = 0.0f32;

                        for record in &records {
                            let date = record.session.started_at.with_timezone(&Local).date_naive();
                            let kj = record.session.kilojoules();
                            let dur = record.session.duration_secs();
                            if date >= week_start {
                                wk_kj += kj;
                                wk_secs += dur;
                            }
                            if date >= month_start {
                                mo_kj += kj;
                            }
                        }
                        for act in &icu_activities {
                            let kj = act
                                .average_watts
                                .and_then(|w| {
                                    act.duration_secs.map(|d| w as f32 * d as f32 / 1000.0)
                                })
                                .unwrap_or(0.0);
                            let dur = act.duration_secs.unwrap_or(0) as u64;
                            if act.date >= week_start {
                                wk_kj += kj;
                                wk_secs += dur;
                            }
                            if act.date >= month_start {
                                mo_kj += kj;
                            }
                        }

                        let has_sessions = !records.is_empty() || !icu_activities.is_empty();
                        volume_section_r.set_visible(has_sessions);
                        week_kj_label.set_label(&format!("{:.0} kJ", wk_kj));
                        week_hrs_label.set_label(&format!("{:.1} h", wk_secs as f32 / 3600.0));
                        month_kj_label.set_label(&format!("{:.0} kJ", mo_kj));
                        total_sessions_label
                            .set_label(&(records.len() + icu_activities.len()).to_string());

                        // Weekly TSS chart (last 6 weeks, oldest → newest)
                        let mut week_tss: Vec<(String, f32)> = Vec::with_capacity(6);
                        for i in (0..6i64).rev() {
                            let ws = week_start - Duration::weeks(i);
                            let we = ws + Duration::days(6);
                            let tss_sessions: f32 = records
                                .iter()
                                .filter(|r| {
                                    let d = r.session.started_at.with_timezone(&Local).date_naive();
                                    d >= ws && d <= we
                                })
                                .filter_map(|r| r.session.tss(ftp_val))
                                .sum();
                            let tss_icu: f32 = intervals_pairs
                                .iter()
                                .filter(|(d, _)| *d >= ws && *d <= we)
                                .map(|(_, t)| *t)
                                .sum();
                            let tss = tss_sessions + tss_icu;
                            let iso_week = ws.iso_week().week();
                            week_tss.push((format!("W{iso_week}"), tss));
                        }

                        for (i, (label_text, tss_val)) in week_tss.iter().enumerate() {
                            week_header_labels[i].set_label(label_text);
                            if *tss_val > 0.0 {
                                tss_value_labels[i].set_label(&format!("{:.0}", tss_val));
                            } else {
                                tss_value_labels[i].set_label("—");
                            }
                        }
                        *tss_week_data.borrow_mut() = week_tss.iter().map(|(_, t)| *t).collect();
                        tss_chart.queue_draw();

                        // Zone distribution. Each ride is bucketed against the FTP
                        // it was ridden at, so raising FTP does not retroactively
                        // demote past efforts into lower zones — time_in_zones
                        // falls back to ftp_val only for rides with no stamp.
                        let mut zone_secs = [0u32; 7];
                        for record in &records {
                            for (zone, secs) in
                                record.session.time_in_zones(ftp_val).iter().enumerate()
                            {
                                zone_secs[zone] += secs;
                            }
                        }
                        let has_power = zone_secs.iter().any(|&s| s > 0);
                        zone_section.set_visible(has_power);
                        if has_power {
                            *zone_seconds.borrow_mut() = zone_secs;
                            zone_bar.queue_draw();
                        }

                        // Power curve
                        let cutoff_30d = today - Duration::days(30);
                        let mut all_time_peaks = vec![0u32; CURVE_DURATIONS.len()];
                        let mut month_peaks = vec![0u32; CURVE_DURATIONS.len()];
                        for record in &records {
                            let date = record.session.started_at.with_timezone(&Local).date_naive();
                            let is_recent = date >= cutoff_30d;
                            for (i, &dur) in CURVE_DURATIONS.iter().enumerate() {
                                if let Some(peak) = record.session.peak_power_for_duration(dur) {
                                    if peak > all_time_peaks[i] {
                                        all_time_peaks[i] = peak;
                                    }
                                    if is_recent && peak > month_peaks[i] {
                                        month_peaks[i] = peak;
                                    }
                                }
                            }
                        }
                        let has_curve = all_time_peaks.iter().any(|&p| p > 0);
                        curve_section_r.set_visible(has_curve);
                        if has_curve {
                            *curve_data_r.borrow_mut() = all_time_peaks
                                .iter()
                                .zip(month_peaks.iter())
                                .map(|(&a, &m)| (a, m))
                                .collect();
                            for (lbl, &peak) in curve_w_labels_r.iter().zip(all_time_peaks.iter()) {
                                if peak > 0 {
                                    lbl.set_label(&format!("{}W", peak));
                                } else {
                                    lbl.set_label("—");
                                }
                            }
                            curve_chart_r.queue_draw();
                        }

                        // Pace curve (running activities with cached streams)
                        let pace_curve = compute_pace_curve(&run_streams, cutoff_30d);
                        let has_pace = pace_curve.iter().any(|&(a, _)| a > 0);
                        pace_section_r.set_visible(has_pace);
                        if has_pace {
                            for (lbl, &(all_p, _)) in
                                pace_val_labels_r.iter().zip(pace_curve.iter())
                            {
                                if all_p > 0 {
                                    lbl.set_label(&format!("{}/km", format_pace_display(all_p)));
                                } else {
                                    lbl.set_label("—");
                                }
                            }
                            *pace_data_r.borrow_mut() = pace_curve;
                            pace_chart_r.queue_draw();
                        }

                        // Heart rate zones (local sessions)
                        let max_hr = athlete_r.borrow().max_hr;
                        let hr_zones = compute_hr_zones(&records, max_hr);
                        let has_hr = hr_zones.iter().any(|&s| s > 0);
                        hr_zone_section_r.set_visible(has_hr);
                        if has_hr {
                            *hr_zone_seconds_r.borrow_mut() = hr_zones;
                            hr_zone_bar_r.queue_draw();
                        }

                        // Section wrappers show when any of their children have data
                        zones_section_r.set_visible(has_power || has_hr);
                        bests_section_r.set_visible(has_curve || has_pace);

                        // Restore cached AI fitness insight if present
                        if !cached_insight.trim().is_empty() {
                            populate_ai_content(&ai_content_r, &analyse_label_r, &cached_insight);
                        }
                    },
                );
            })
        };

        // ── Analyse Fitness button ────────────────────────────────────────────
        {
            let pool_a = pool.clone();
            let rt_a = rt_handle.clone();
            let ftp_a = Rc::clone(&ftp);
            let athlete_a = Rc::clone(&athlete);
            let tss_week_a = Rc::clone(&tss_week_data);
            let label_a = analyse_label.clone();
            let ai_content = ai_content.clone();
            let spinner_a = analyse_spinner.clone();

            analyse_btn.connect_clicked(move |btn| {
                let api_key = match keystore::get_secret(keystore::KEY_ANTHROPIC) {
                    Ok(Some(k)) if !k.trim().is_empty() => k,
                    _ => {
                        label_a.set_text(
                            "No API key configured. Enter your Anthropic API key in \
                                 Preferences → Integrations.",
                        );
                        label_a.remove_css_class("dim-label");
                        return;
                    }
                };

                // Read the !Send shared state on the main thread before spawning.
                let ftp_val = ftp_a.get();
                let week_tss = tss_week_a.borrow().clone();
                let athlete = athlete_a.borrow().clone();

                let btn_c = btn.clone();
                btn.set_sensitive(false);
                spinner_a.set_visible(true);
                spinner_a.start();
                label_a.set_text("Asking the AI Coach to analyse your fitness metrics…");
                label_a.remove_css_class("dim-label");

                let (tx, rx) = async_channel::bounded::<Result<String, String>>(1);
                let pool_t = pool_a.clone();
                // All DB reads + prompt assembly + the network call run off the main
                // thread, so the click never blocks the GLib loop (CLAUDE.md §2.3).
                rt_a.spawn(async move {
                    let records = db::load_session_summaries(&pool_t)
                        .await
                        .unwrap_or_default();
                    let intervals_pairs = db::load_intervals_tss_pairs(&pool_t)
                        .await
                        .unwrap_or_default();
                    let icu_count =
                        db::count_intervals_activities(&pool_t).await.unwrap_or(0) as usize;
                    let wellness_raw = db::load_wellness_recent(&pool_t, 7)
                        .await
                        .unwrap_or_default();
                    let athlete_context = db::get_setting(&pool_t, "coaching.athlete_context")
                        .await
                        .unwrap_or(None)
                        .unwrap_or_default();

                    let today = Local::now().date_naive();
                    let (ctl, atl, ctl_4wk_ago) =
                        compute_load_metrics(&records, &intervals_pairs, ftp_val, today);
                    let tsb = ctl - atl;

                    let wellness: Vec<WellnessSnapshot> = wellness_raw
                        .iter()
                        .map(|w| WellnessSnapshot {
                            date: w.date.format("%Y-%m-%d").to_string(),
                            hrv: w.hrv,
                            resting_hr: w.resting_hr,
                            sleep_hours: w.sleep_secs.map(|s| s as f32 / 3600.0),
                            sleep_score: w.sleep_score,
                            steps: w.steps,
                            calories: w.calories,
                        })
                        .collect();

                    let ctx = FitnessContext {
                        athlete,
                        ctl,
                        atl,
                        tsb,
                        ctl_4wk_ago,
                        week_tss,
                        total_sessions: records.len() + icu_count,
                        athlete_context,
                        wellness,
                    };
                    let prompt = build_fitness_prompt(&ctx);

                    let result = get_suggestion(&api_key, &prompt, 1024)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx.send(result).await;
                });

                let label_c = label_a.clone();
                let ai_content_c = ai_content.clone();
                let spinner_c = spinner_a.clone();
                let pool_cache = pool_a.clone();
                let rt_cache = rt_a.clone();
                glib::MainContext::default().spawn_local(async move {
                    if let Ok(result) = rx.recv().await {
                        match result {
                            Ok(text) => {
                                populate_ai_content(&ai_content_c, &label_c, &text);
                                let pool_cc = pool_cache.clone();
                                rt_cache.spawn(async move {
                                    let _ = db::set_setting(&pool_cc, "ai.fitness_insight", &text)
                                        .await;
                                });
                            }
                            Err(e) => {
                                tracing::error!("AI fitness analysis failed: {e}");
                                label_c.set_text(
                                    "The AI Coach couldn't complete this request. \
                                     Please check your API key and try again.",
                                );
                            }
                        }
                    }
                    spinner_c.stop();
                    spinner_c.set_visible(false);
                    btn_c.set_sensitive(true);
                });
            });
        }

        // ── Retrospectives: cached restore + shared Generate button ───────────
        for (content, label, cache_key) in [
            (&weekly_content, &weekly_label, "ai.weekly_retrospective"),
            (&monthly_content, &monthly_label, "ai.monthly_retrospective"),
        ] {
            // Restore cached retrospective (loaded off the main thread — CLAUDE.md §2.3)
            let pool_load = pool.clone();
            let cache_key_load = cache_key.to_string();
            let content_c = content.clone();
            let label_c = label.clone();
            crate::ui::spawn_to_main(
                &rt_handle,
                async move {
                    db::get_setting(&pool_load, &cache_key_load)
                        .await
                        .unwrap_or(None)
                        .unwrap_or_default()
                },
                move |cached| {
                    if !cached.is_empty() {
                        populate_ai_content(&content_c, &label_c, &cached);
                    }
                },
            );
        }

        // One Generate button serves both periods; dispatch on the active toggle.
        let run_retro: Rc<dyn Fn(RetroPeriod)> = {
            let pool_r = pool.clone();
            let rt_r = rt_handle.clone();
            let athlete_r = Rc::clone(&athlete);
            let ftp_r = Rc::clone(&ftp);
            let weekly = (weekly_content.clone(), weekly_label.clone());
            let monthly = (monthly_content.clone(), monthly_label.clone());
            let spinner_r = retro_spinner.clone();
            let generate_btn = generate_btn.clone();
            Rc::new(move |period: RetroPeriod| {
                let (content_r, label_r, cache_key_s) = match period {
                    RetroPeriod::Weekly => (&weekly.0, &weekly.1, "ai.weekly_retrospective"),
                    RetroPeriod::Monthly => (&monthly.0, &monthly.1, "ai.monthly_retrospective"),
                };
                let api_key = match keystore::get_secret(keystore::KEY_ANTHROPIC) {
                    Ok(Some(k)) if !k.trim().is_empty() => k,
                    _ => {
                        label_r.set_text(
                            "No API key configured. Enter your Anthropic API key in \
                                 Preferences → Integrations.",
                        );
                        label_r.remove_css_class("dim-label");
                        return;
                    }
                };

                let today = Local::now().date_naive();
                let period_days = match period {
                    RetroPeriod::Weekly => 7i64,
                    RetroPeriod::Monthly => 30i64,
                };
                let start_date = today - Duration::days(period_days - 1);

                let start_utc = start_date
                    .and_hms_opt(0, 0, 0)
                    .expect("midnight is always valid")
                    .and_utc()
                    .to_rfc3339();
                let end_utc = today
                    .and_hms_opt(23, 59, 59)
                    .expect("end of day is always valid")
                    .and_utc()
                    .to_rfc3339();

                // Read !Send shared state on the main thread before spawning.
                let ftp_val = ftp_r.get();
                let athlete = athlete_r.borrow().clone();

                generate_btn.set_sensitive(false);
                spinner_r.set_visible(true);
                spinner_r.start();
                label_r.set_text(&format!(
                    "Generating {} retrospective analysis…",
                    period.label()
                ));
                label_r.remove_css_class("dim-label");

                let (tx, rx) = async_channel::bounded::<Result<String, String>>(1);
                let pool_t = pool_r.clone();
                // All DB reads + prompt assembly + the network call run off the main
                // thread, so the click never blocks the GLib loop (CLAUDE.md §2.3).
                rt_r.spawn(async move {
                    let records = db::load_sessions_between(&pool_t, &start_utc, &end_utc)
                        .await
                        .unwrap_or_default();
                    let icu_acts = db::load_unlinked_intervals_activities(&pool_t)
                        .await
                        .unwrap_or_default();
                    let intervals_all = db::load_intervals_tss_pairs(&pool_t)
                        .await
                        .unwrap_or_default();
                    let wellness_raw = db::load_wellness_between(
                        &pool_t,
                        &start_date.format("%Y-%m-%d").to_string(),
                        &today.format("%Y-%m-%d").to_string(),
                    )
                    .await
                    .unwrap_or_default();
                    let all_records = db::load_session_records(&pool_t).await.unwrap_or_default();
                    let athlete_context = db::get_setting(&pool_t, "coaching.athlete_context")
                        .await
                        .unwrap_or(None)
                        .unwrap_or_default();

                    let all_rides: Vec<db::SessionSummary> =
                        all_records.iter().map(|r| r.summary()).collect();
                    let (ctl_end, atl_end, _) =
                        compute_load_metrics(&all_rides, &intervals_all, ftp_val, today);
                    let ctl_start_date = today - Duration::days(period_days);
                    let (ctl_start, _, _) =
                        compute_load_metrics(&all_rides, &intervals_all, ftp_val, ctl_start_date);
                    let tsb_end = ctl_end - atl_end;

                    let mut sessions: Vec<RetroSession> = Vec::new();
                    for r in &records {
                        let date = r.session.started_at.with_timezone(&Local).date_naive();
                        sessions.push(RetroSession {
                            date: date.format("%Y-%m-%d").to_string(),
                            name: r.workout_name.clone(),
                            sport_type: "Cycling".to_string(),
                            duration_mins: r.session.duration_secs() as u32 / 60,
                            avg_power: r.session.average_power().map(|p| p as u32),
                            tss: r.session.tss(ftp_val),
                            kj: r.session.kilojoules(),
                        });
                    }
                    for act in &icu_acts {
                        if act.date >= start_date && act.date <= today {
                            let sport = normalize_sport_type(&act.sport_type);
                            let is_cycling = sport == "Cycling";
                            sessions.push(RetroSession {
                                date: act.date.format("%Y-%m-%d").to_string(),
                                name: if act.name.is_empty() {
                                    None
                                } else {
                                    Some(act.name.clone())
                                },
                                sport_type: sport,
                                duration_mins: act.duration_secs.unwrap_or(0) / 60,
                                avg_power: if is_cycling { act.average_watts } else { None },
                                tss: act.tss,
                                kj: if is_cycling {
                                    act.average_watts
                                        .and_then(|w| {
                                            act.duration_secs.map(|d| w as f32 * d as f32 / 1000.0)
                                        })
                                        .unwrap_or(0.0)
                                } else {
                                    0.0
                                },
                            });
                        }
                    }
                    sessions.sort_by_key(|s| s.date.clone());

                    let wellness: Vec<WellnessSnapshot> = wellness_raw
                        .iter()
                        .map(|w| WellnessSnapshot {
                            date: w.date.format("%Y-%m-%d").to_string(),
                            hrv: w.hrv,
                            resting_hr: w.resting_hr,
                            sleep_hours: w.sleep_secs.map(|s| s as f32 / 3600.0),
                            sleep_score: w.sleep_score,
                            steps: w.steps,
                            calories: w.calories,
                        })
                        .collect();

                    let ctx = RetrospectiveContext {
                        athlete,
                        period,
                        sessions,
                        wellness,
                        ctl_start,
                        ctl_end,
                        atl_end,
                        tsb_end,
                        athlete_context,
                    };
                    let prompt = build_retrospective_prompt(&ctx);

                    let r = get_suggestion(&api_key, &prompt, 1500)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx.send(r).await;
                });

                let label_c = label_r.clone();
                let content_c = content_r.clone();
                let spinner_c = spinner_r.clone();
                let btn_c = generate_btn.clone();
                let pool_c = pool_r.clone();
                let rt_c = rt_r.clone();
                let key_c = cache_key_s.to_string();
                glib::MainContext::default().spawn_local(async move {
                    if let Ok(result) = rx.recv().await {
                        match result {
                            Ok(text) => {
                                populate_ai_content(&content_c, &label_c, &text);
                                rt_c.spawn(async move {
                                    let _ = db::set_setting(&pool_c, &key_c, &text).await;
                                });
                            }
                            Err(e) => {
                                tracing::error!("AI retrospective failed: {e}");
                                label_c.set_text(
                                    "The AI Coach couldn't complete this request. \
                                     Please check your API key and try again.",
                                );
                            }
                        }
                    }
                    spinner_c.stop();
                    spinner_c.set_visible(false);
                    btn_c.set_sensitive(true);
                });
            })
        };
        {
            let run_retro = Rc::clone(&run_retro);
            let month_toggle = month_toggle.clone();
            generate_btn.connect_clicked(move |_| {
                let period = if month_toggle.is_active() {
                    RetroPeriod::Monthly
                } else {
                    RetroPeriod::Weekly
                };
                run_retro(period);
            });
        }

        reload();

        (Self { root }, reload)
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }
}

/// Build a 14-element series aligned to [today-13 .. today], 0.0 = no data.
fn build_14day_series(
    wellness: &[db::WellnessEntry],
    today: NaiveDate,
    extractor: impl Fn(&db::WellnessEntry) -> Option<f32>,
) -> Vec<f32> {
    let mut vals = vec![0.0f32; 14];
    for entry in wellness {
        let days_ago = (today - entry.date).num_days();
        if (0..14).contains(&days_ago) {
            let idx = (13 - days_ago) as usize;
            if let Some(v) = extractor(entry) {
                if v > 0.0 {
                    vals[idx] = v;
                }
            }
        }
    }
    vals
}

/// Update one wellness card: value label, trend label, sparkline data, and redraw.
/// `fmt` is a format string fragment like `"{:.0}"` or `"{:.1}"`.
/// `higher_is_better` controls whether above-average gets `success` or `warning`.
fn update_wellness_card(
    value_lbl: &gtk::Label,
    trend_lbl: &gtk::Label,
    data: &Rc<RefCell<Vec<f32>>>,
    chart: &gtk::DrawingArea,
    series: &[f32],
    fmt: &str,
    higher_is_better: bool,
) {
    let current = series
        .iter()
        .rev()
        .find(|&&v| v > 0.0)
        .copied()
        .unwrap_or(0.0);

    if current > 0.0 {
        let formatted = if fmt.contains(".1") {
            format!("{:.1}", current)
        } else {
            format!("{:.0}", current)
        };
        value_lbl.set_label(&formatted);
    } else {
        value_lbl.set_label("—");
    }

    // Trend vs 14-day average
    let valid: Vec<f32> = series.iter().filter(|&&v| v > 0.0).copied().collect();
    trend_lbl.remove_css_class("success");
    trend_lbl.remove_css_class("warning");
    if valid.len() >= 3 && current > 0.0 {
        let avg = valid.iter().sum::<f32>() / valid.len() as f32;
        let delta = current - avg;
        let pct = (delta / avg * 100.0).abs();
        if delta.abs() < avg * 0.03 {
            trend_lbl.set_label("→ avg");
        } else if delta > 0.0 {
            trend_lbl.set_label(&format!("↑ {pct:.0}% above avg"));
            trend_lbl.add_css_class(if higher_is_better {
                "success"
            } else {
                "warning"
            });
        } else {
            trend_lbl.set_label(&format!("↓ {pct:.0}% below avg"));
            trend_lbl.add_css_class(if higher_is_better {
                "warning"
            } else {
                "success"
            });
        }
    } else {
        trend_lbl.set_label("");
    }

    *data.borrow_mut() = series.to_vec();
    chart.queue_draw();
}

impl FitnessPage {
    /// Create a wellness sparkline card. Returns `(card, value_label, trend_label, chart, data)`.
    fn make_wellness_card(
        title: &str,
        unit: &str,
    ) -> (
        gtk::Box,
        gtk::Label,
        gtk::Label,
        gtk::DrawingArea,
        Rc<RefCell<Vec<f32>>>,
    ) {
        let card = gtk::Box::builder()
            .css_classes(["card"])
            .hexpand(true)
            .build();

        let vbox = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        vbox.append(
            &gtk::Label::builder()
                .label(title)
                .halign(gtk::Align::Start)
                .css_classes(["caption", "dim-label"])
                .build(),
        );

        let value_lbl = gtk::Label::builder()
            .label("—")
            .halign(gtk::Align::Start)
            .css_classes(["title-2", "numeric"])
            .build();
        vbox.append(&value_lbl);

        if !unit.is_empty() {
            vbox.append(
                &gtk::Label::builder()
                    .label(unit)
                    .halign(gtk::Align::Start)
                    .css_classes(["caption", "dim-label"])
                    .build(),
            );
        }

        // Sparkline — "accent" class makes widget.color() resolve to the
        // GNOME accent colour (see the PMC chart note)
        let data: Rc<RefCell<Vec<f32>>> = Rc::new(RefCell::new(vec![]));
        let chart = gtk::DrawingArea::builder()
            .content_height(42)
            .hexpand(true)
            .css_classes(["accent"])
            .build();
        let data_ref = Rc::clone(&data);
        chart.set_draw_func(move |widget, cr, width, height| {
            let fg = widget.color();
            let (fr, fgr, fb) = (fg.red() as f64, fg.green() as f64, fg.blue() as f64);
            let vals = data_ref.borrow();
            let points: Vec<(usize, f32)> = vals
                .iter()
                .enumerate()
                .filter_map(|(i, &v)| if v > 0.0 { Some((i, v)) } else { None })
                .collect();
            if points.len() < 2 {
                return;
            }
            let min_v = points.iter().map(|&(_, v)| v).fold(f32::MAX, f32::min);
            let max_v = points.iter().map(|&(_, v)| v).fold(f32::MIN, f32::max);
            let range = (max_v - min_v).max(1.0);
            let w = width as f64;
            let h = height as f64;
            let n = (vals.len() - 1).max(1) as f64;
            let pad = 4.0f64;

            let px = |i: usize| (i as f64 / n) * w;
            let py = |v: f32| h - pad - ((v - min_v) as f64 / range as f64) * (h - pad * 2.0);

            // Fill under line
            cr.set_source_rgba(fr, fgr, fb, 0.10);
            let (fi, fv) = points[0];
            cr.move_to(px(fi), h);
            cr.line_to(px(fi), py(fv));
            for &(i, v) in &points[1..] {
                cr.line_to(px(i), py(v));
            }
            let (li, _) = *points.last().expect("len >= 2");
            cr.line_to(px(li), h);
            cr.close_path();
            cr.fill().ok();

            // Line
            cr.set_source_rgba(fr, fgr, fb, 0.80);
            cr.set_line_width(2.0);
            cr.move_to(px(fi), py(fv));
            for &(i, v) in &points[1..] {
                cr.line_to(px(i), py(v));
            }
            cr.stroke().ok();

            // Dot at latest point
            let (ldi, ldv) = *points.last().expect("len >= 2");
            cr.set_source_rgba(fr, fgr, fb, 1.0);
            cr.arc(px(ldi), py(ldv), 3.5, 0.0, std::f64::consts::TAU);
            cr.fill().ok();
        });
        vbox.append(&chart);

        let trend_lbl = gtk::Label::builder()
            .label("")
            .halign(gtk::Align::Start)
            .css_classes(["caption"])
            .build();
        vbox.append(&trend_lbl);

        card.append(&vbox);
        (card, value_lbl, trend_lbl, chart, data)
    }
}

/// Populate the AI content container with structured sections if the text contains
/// numbered section headings (e.g. "1. Current state:"), otherwise show the fallback label.
fn populate_ai_content(container: &gtk::Box, fallback_label: &gtk::Label, text: &str) {
    // Remove all children except the fallback label (which is always child 0).
    while let Some(child) = container.last_child() {
        if child == *fallback_label.upcast_ref::<gtk::Widget>() {
            break;
        }
        container.remove(&child);
    }

    // Try to detect numbered sections like "1. **Heading**:" or "1. Heading:"
    let sections = parse_ai_sections(text);

    if sections.is_empty() {
        // Unstructured text — render as formatted markdown in the fallback label
        fallback_label.set_markup(&to_pango(text));
        fallback_label.remove_css_class("dim-label");
        fallback_label.set_visible(true);
        return;
    }

    fallback_label.set_visible(false);

    for (heading, body) in sections {
        let sep = gtk::Separator::new(gtk::Orientation::Horizontal);
        container.append(&sep);

        let section_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .margin_top(10)
            .margin_bottom(10)
            .margin_start(12)
            .margin_end(12)
            .build();

        let heading_label = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .css_classes(["caption-heading"])
            .build();
        heading_label.set_markup(&to_pango(&heading));
        section_box.append(&heading_label);

        let body_label = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .wrap(true)
            .selectable(true)
            .xalign(0.0)
            .build();
        body_label.set_markup(&to_pango(&body));
        section_box.append(&body_label);

        container.append(&section_box);
    }
}

/// Parse AI text into (heading, body) pairs by detecting lines like "1. **Heading**:" or "1. Heading:".
/// Returns empty vec if no numbered sections are detected.
fn parse_ai_sections(text: &str) -> Vec<(String, String)> {
    let mut sections: Vec<(String, String)> = Vec::new();
    let mut current_heading = String::new();
    let mut current_body: Vec<&str> = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim();
        // Detect lines like "1. **Training Load Summary**:" or "2. Trend"
        // Allow up to 100 chars to accommodate markdown decoration in the heading
        let is_section_head = trimmed.len() < 100
            && trimmed.chars().next().is_some_and(|c| c.is_ascii_digit())
            && trimmed.contains(". ")
            && !trimmed.starts_with("0.");

        if is_section_head {
            if !current_heading.is_empty() {
                let body = current_body.join("\n").trim().to_string();
                if !body.is_empty() {
                    sections.push((current_heading.clone(), body));
                }
                current_body.clear();
            }
            // Extract the heading text after "N. ", strip trailing colon
            let label = trimmed
                .split_once(". ")
                .map(|x| x.1)
                .unwrap_or(trimmed)
                .trim_end_matches(':')
                .to_string();
            current_heading = label;
        } else if !current_heading.is_empty() {
            current_body.push(trimmed);
        }
    }
    if !current_heading.is_empty() {
        let body = current_body.join("\n").trim().to_string();
        if !body.is_empty() {
            sections.push((current_heading, body));
        }
    }

    // Need at least 2 sections to be worth showing structured
    if sections.len() < 2 {
        return Vec::new();
    }

    sections
}

/// Build one retrospective period's content area for the Coach card stack.
/// Returns `(content_box, fallback_label)`.
fn build_retro_content(period_desc: &str) -> (gtk::Box, gtk::Label) {
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(0)
        .build();

    let label = gtk::Label::builder()
        .label(format!(
            "Select Generate for an AI retrospective of the {period_desc}."
        ))
        .css_classes(["dim-label"])
        .halign(gtk::Align::Start)
        .wrap(true)
        .selectable(true)
        .xalign(0.0)
        .margin_top(10)
        .margin_bottom(10)
        .margin_start(12)
        .margin_end(12)
        .build();
    content.append(&label);
    (content, label)
}
