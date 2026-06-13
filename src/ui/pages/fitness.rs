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

const HR_ZONE_COLORS: [(f64, f64, f64); 5] = [
    (0.34, 0.89, 0.53), // Z1 Easy
    (0.47, 0.68, 0.93), // Z2 Aerobic
    (0.97, 0.89, 0.36), // Z3 Tempo
    (1.00, 0.48, 0.39), // Z4 Threshold
    (0.84, 0.20, 0.20), // Z5 Max
];

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
    records: &[db::SessionRecord],
    intervals_pairs: &[(NaiveDate, f32)],
    ftp: u32,
    today: NaiveDate,
) -> (f64, f64, f64) {
    let mut daily_tss: HashMap<NaiveDate, f32> = HashMap::new();
    for record in records {
        // Skip sessions that were uploaded to Intervals.icu — their TSS is
        // already included via intervals_pairs, so counting them here too would
        // inflate CTL/ATL by double-counting the same workout.
        if record.uploaded_to_icu {
            continue;
        }
        let date = record.session.started_at.with_timezone(&Local).date_naive();
        if let Some(tss) = record.session.tss(ftp) {
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
    records: &[db::SessionRecord],
    intervals_pairs: &[(NaiveDate, f32)],
    ftp: u32,
    today: NaiveDate,
) -> Vec<PmcPoint> {
    let mut daily_tss: HashMap<NaiveDate, f32> = HashMap::new();
    for record in records {
        if record.uploaded_to_icu {
            continue;
        }
        let date = record.session.started_at.with_timezone(&Local).date_naive();
        if let Some(tss) = record.session.tss(ftp) {
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

fn ctl_status_text(ctl: f64) -> &'static str {
    match ctl as u32 {
        0..=15 => "Building your aerobic base",
        16..=30 => "Moderate aerobic base",
        31..=50 => "Good aerobic fitness",
        51..=70 => "Strong fitness level",
        _ => "Very high fitness",
    }
}

fn atl_status_text(atl: f64) -> &'static str {
    match atl as u32 {
        0..=15 => "Low recent load — well rested",
        16..=30 => "Moderate recent load",
        31..=50 => "High recent load",
        _ => "Very high load — monitor recovery",
    }
}

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

fn form_summary_text(ctl: f64, tsb: f64) -> String {
    let fitness_phrase = if ctl < 16.0 {
        "building your aerobic base"
    } else if ctl < 31.0 {
        "developing a moderate aerobic base"
    } else if ctl < 51.0 {
        "maintaining a solid aerobic base"
    } else {
        "maintaining a strong fitness level"
    };

    let form_phrase = if tsb > 25.0 {
        "and feeling very fresh — you could increase training volume"
    } else if tsb > 5.0 {
        "and in good form — ideal for quality sessions"
    } else if tsb > -10.0 {
        "while carrying normal training fatigue"
    } else if tsb > -30.0 {
        "while accumulating fatigue — an easier day or two would help"
    } else {
        "while significantly fatigued — rest is the priority"
    };

    format!("You are {} {}.", fitness_phrase, form_phrase)
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

        // ── AI Coach card — top of page, first thing the user sees ───────────
        let ai_card = gtk::Box::builder()
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
                .label("AI Coach")
                .css_classes(["heading"])
                .halign(gtk::Align::Start)
                .hexpand(true)
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
        ai_card.append(&ai_header);
        ai_card.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

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
        ai_card.append(&ai_content);
        inner.append(&ai_card);

        // ── Training Load ─────────────────────────────────────────────────────
        inner.append(
            &gtk::Label::builder()
                .label("Training Load")
                .halign(gtk::Align::Start)
                .css_classes(["heading"])
                .build(),
        );
        inner.append(
            &gtk::Label::builder()
                .label(
                    "CTL (fitness), ATL (fatigue), and TSB (form) are exponential moving \
                     averages of your daily training stress. Together they describe where you \
                     are in your fitness-fatigue cycle.",
                )
                .css_classes(["dim-label"])
                .halign(gtk::Align::Start)
                .wrap(true)
                .build(),
        );

        let icu_indicator = gtk::Label::builder()
            .label("")
            .css_classes(["caption", "dim-label"])
            .halign(gtk::Align::Start)
            .visible(false)
            .build();
        inner.append(&icu_indicator);

        let metrics_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .homogeneous(true)
            .build();

        let (ctl_frame, ctl_label, ctl_status) =
            Self::make_metric_card("CTL", "Chronic Training Load");
        let (atl_frame, atl_label, atl_status) =
            Self::make_metric_card("ATL", "Acute Training Load");
        let (tsb_frame, tsb_label, tsb_status) =
            Self::make_metric_card("TSB", "Training Stress Balance");
        metrics_row.append(&ctl_frame);
        metrics_row.append(&atl_frame);
        metrics_row.append(&tsb_frame);
        inner.append(&metrics_row);

        let form_summary = gtk::Label::builder()
            .label("")
            .halign(gtk::Align::Start)
            .wrap(true)
            .css_classes(["dim-label"])
            .visible(false)
            .build();
        inner.append(&form_summary);

        // ── Performance Management Chart ──────────────────────────────────────
        // (date, ctl, atl, tsb) series for the past 90 days
        let pmc_data: Rc<RefCell<Vec<PmcPoint>>> = Rc::new(RefCell::new(Vec::new()));

        let pmc_chart = gtk::DrawingArea::builder()
            .content_height(170)
            .hexpand(true)
            .accessible_role(gtk::AccessibleRole::Img)
            .build();
        pmc_chart.update_property(&[gtk::accessible::Property::Label(
            "Performance Management Chart: 90-day history of fitness (CTL), fatigue (ATL), and form (TSB)",
        )]);

        {
            let pd = Rc::clone(&pmc_data);
            pmc_chart.set_draw_func(move |_w, cr, width, height| {
                let data = pd.borrow();
                if data.len() < 2 {
                    return;
                }
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
                cr.set_source_rgba(0.5, 0.5, 0.5, 0.25);
                cr.set_line_width(1.0);
                cr.move_to(0.0, zero_y);
                cr.line_to(w, zero_y);
                cr.stroke().ok();

                // TSB fill: green above zero, warm red below zero
                {
                    cr.new_path();
                    cr.move_to(x_at(0), zero_y);
                    for (i, &(_, _, _, s)) in data.iter().enumerate() {
                        cr.line_to(x_at(i), y_at(s));
                    }
                    cr.line_to(x_at(n - 1), zero_y);
                    cr.close_path();
                    cr.set_source_rgba(0.30, 0.75, 0.55, 0.20);
                    cr.fill().ok();
                }

                // Draw a single series as a line — field_idx: 1=CTL, 2=ATL, 3=TSB
                let draw_series = |field_idx: usize, r: f64, g: f64, b: f64| {
                    let vals: Vec<f64> = data
                        .iter()
                        .map(|&(_, c, a, s)| match field_idx {
                            1 => c,
                            2 => a,
                            _ => s,
                        })
                        .collect();
                    cr.set_source_rgba(r, g, b, 0.90);
                    cr.set_line_width(2.0);
                    cr.move_to(x_at(0), y_at(vals[0]));
                    for (i, &v) in vals.iter().enumerate().skip(1) {
                        cr.line_to(x_at(i), y_at(v));
                    }
                    cr.stroke().ok();
                };

                // ATL (amber) — draw first so CTL renders on top
                draw_series(2, 1.0, 0.70, 0.20);
                // CTL (blue)
                draw_series(1, 0.47, 0.68, 0.93);
                // TSB (teal)
                draw_series(3, 0.30, 0.80, 0.65);

                // X-axis: draw a tick and short month label at the 1st of each month
                cr.set_source_rgba(0.5, 0.5, 0.5, 0.55);
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

        let pmc_legend = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(18)
            .build();
        for (label, css) in [
            ("● CTL (fitness)", "accent"),
            ("● ATL (fatigue)", "warning"),
            ("● TSB (form)", "success"),
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
        pmc_section.append(
            &gtk::Label::builder()
                .label("Performance Management Chart")
                .halign(gtk::Align::Start)
                .css_classes(["heading"])
                .build(),
        );
        pmc_section.append(
            &gtk::Label::builder()
                .label(
                    "90-day history of fitness (CTL), fatigue (ATL), and form (TSB = CTL − ATL).",
                )
                .css_classes(["dim-label"])
                .halign(gtk::Align::Start)
                .wrap(true)
                .build(),
        );
        pmc_section.append(&pmc_chart);
        pmc_section.append(&pmc_legend);
        inner.append(&pmc_section);

        // ── Wellness ──────────────────────────────────────────────────────────
        inner.append(
            &gtk::Label::builder()
                .label("Wellness")
                .halign(gtk::Align::Start)
                .css_classes(["heading"])
                .build(),
        );
        inner.append(
            &gtk::Label::builder()
                .label(
                    "HRV, resting heart rate, sleep, and activity data synced from \
                     Intervals.icu. Use Preferences → Intervals.icu to sync.",
                )
                .css_classes(["dim-label"])
                .halign(gtk::Align::Start)
                .wrap(true)
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

        // ── Weekly TSS ────────────────────────────────────────────────────────
        inner.append(
            &gtk::Label::builder()
                .label("Weekly Training Stress")
                .halign(gtk::Align::Start)
                .css_classes(["heading"])
                .build(),
        );
        inner.append(
            &gtk::Label::builder()
                .label(
                    "TSS (Training Stress Score) quantifies each session's overall load — \
                     higher bars mean harder weeks. A sustainable build is roughly 5–10% \
                     per week.",
                )
                .css_classes(["dim-label"])
                .halign(gtk::Align::Start)
                .wrap(true)
                .build(),
        );

        let tss_week_data: Rc<RefCell<Vec<f32>>> = Rc::new(RefCell::new(vec![0.0; 6]));

        let tss_chart = gtk::DrawingArea::builder()
            .content_height(120)
            .hexpand(true)
            .accessible_role(gtk::AccessibleRole::Img)
            .build();
        tss_chart.update_property(&[gtk::accessible::Property::Label(
            "Weekly TSS bar chart: training stress score for the past 6 weeks",
        )]);

        let tss_data_ref = Rc::clone(&tss_week_data);
        tss_chart.set_draw_func(move |_widget, cr, width, height| {
            let weeks = tss_data_ref.borrow();
            let max_tss = weeks.iter().copied().fold(0.0f32, f32::max);
            let w = width as f64;
            let h = height as f64;
            let n = weeks.len() as f64;
            let gap = 6.0;
            let bar_w = ((w - gap * (n - 1.0)) / n).max(1.0);

            for (i, &tss) in weeks.iter().enumerate() {
                let x = i as f64 * (bar_w + gap);
                cr.set_source_rgba(0.47, 0.68, 0.93, 0.15);
                cr.rectangle(x, 0.0, bar_w, h);
                cr.fill().ok();

                if max_tss > 0.0 && tss > 0.0 {
                    let bar_h = (tss as f64 / max_tss as f64 * h).max(2.0);
                    cr.set_source_rgba(0.47, 0.68, 0.93, 0.85);
                    cr.rectangle(x, h - bar_h, bar_w, bar_h);
                    cr.fill().ok();
                }
            }
        });

        inner.append(&tss_chart);

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
        inner.append(&week_label_row);
        inner.append(&tss_value_row);

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
        curve_chart.set_draw_func(move |_w, cr, width, height| {
            let data = curve_data_draw.borrow();
            let max_w = data.iter().map(|&(a, _)| a).max().unwrap_or(0);
            if max_w == 0 {
                return;
            }
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

            // Draw all-time curve (accent yellow)
            let at_pts: Vec<(f64, f64)> = data
                .iter()
                .enumerate()
                .filter(|(_, &(a, _))| a > 0)
                .map(|(i, &(a, _))| (x_at(i), y_at(a)))
                .collect();
            if at_pts.len() >= 2 {
                cr.set_source_rgba(0.97, 0.78, 0.26, 0.85);
                cr.set_line_width(2.0);
                cr.move_to(at_pts[0].0, at_pts[0].1);
                for &(x, y) in &at_pts[1..] {
                    cr.line_to(x, y);
                }
                cr.stroke().ok();
                for &(x, y) in &at_pts {
                    cr.set_source_rgba(0.97, 0.78, 0.26, 1.0);
                    cr.arc(x, y, 3.5, 0.0, std::f64::consts::TAU);
                    cr.fill().ok();
                }
            }

            // Draw 30-day curve (accent blue)
            let mo_pts: Vec<(f64, f64)> = data
                .iter()
                .enumerate()
                .filter(|(_, &(_, m))| m > 0)
                .map(|(i, &(_, m))| (x_at(i), y_at(m)))
                .collect();
            if mo_pts.len() >= 2 {
                cr.set_source_rgba(0.47, 0.68, 0.93, 0.85);
                cr.set_line_width(2.0);
                cr.move_to(mo_pts[0].0, mo_pts[0].1);
                for &(x, y) in &mo_pts[1..] {
                    cr.line_to(x, y);
                }
                cr.stroke().ok();
                for &(x, y) in &mo_pts {
                    cr.set_source_rgba(0.47, 0.68, 0.93, 1.0);
                    cr.arc(x, y, 3.5, 0.0, std::f64::consts::TAU);
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
        let curve_legend = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(18)
            .build();
        curve_legend.append(
            &gtk::Label::builder()
                .label("● All time")
                .css_classes(["caption"])
                .build(),
        );
        let curve_legend_month = gtk::Label::builder()
            .label("● Last 30 days")
            .css_classes(["caption", "accent"])
            .build();
        curve_legend.append(&curve_legend_month);

        let curve_section = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .visible(false)
            .build();
        curve_section.append(
            &gtk::Label::builder()
                .label("Power Curve")
                .halign(gtk::Align::Start)
                .css_classes(["heading"])
                .build(),
        );
        curve_section.append(
            &gtk::Label::builder()
                .label(
                    "Peak average power for each duration across all recorded sessions. \
                     Yellow = all-time best · Blue = last 30 days.",
                )
                .css_classes(["dim-label"])
                .halign(gtk::Align::Start)
                .wrap(true)
                .build(),
        );
        curve_section.append(&curve_chart);
        curve_section.append(&curve_x_row);
        curve_section.append(&curve_w_row);
        curve_section.append(&curve_legend);
        inner.append(&curve_section);

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
            pace_chart.set_draw_func(move |_w, cr, width, height| {
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

                let draw_curve = |pts: &[(f64, f64)], r: f64, g: f64, b: f64| {
                    if pts.len() < 2 {
                        return;
                    }
                    cr.set_source_rgba(r, g, b, 0.85);
                    cr.set_line_width(2.0);
                    cr.move_to(pts[0].0, pts[0].1);
                    for &(x, y) in &pts[1..] {
                        cr.line_to(x, y);
                    }
                    cr.stroke().ok();
                    cr.set_source_rgba(r, g, b, 1.0);
                    for &(x, y) in pts {
                        cr.arc(x, y, 3.5, 0.0, std::f64::consts::TAU);
                        cr.fill().ok();
                    }
                };

                let at_pts: Vec<(f64, f64)> = data
                    .iter()
                    .enumerate()
                    .filter(|(_, &(a, _))| a > 0)
                    .map(|(i, &(a, _))| (x_at(i), y_at(a)))
                    .collect();
                draw_curve(&at_pts, 0.97, 0.78, 0.26);

                let mo_pts: Vec<(f64, f64)> = data
                    .iter()
                    .enumerate()
                    .filter(|(_, &(_, m))| m > 0)
                    .map(|(i, &(_, m))| (x_at(i), y_at(m)))
                    .collect();
                draw_curve(&mo_pts, 0.47, 0.68, 0.93);
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

        let pace_legend = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(18)
            .build();
        pace_legend.append(
            &gtk::Label::builder()
                .label("● All time")
                .css_classes(["caption"])
                .build(),
        );
        pace_legend.append(
            &gtk::Label::builder()
                .label("● Last 30 days")
                .css_classes(["caption", "accent"])
                .build(),
        );

        let pace_section = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .visible(false)
            .build();
        pace_section.append(
            &gtk::Label::builder()
                .label("Pace Curve")
                .halign(gtk::Align::Start)
                .css_classes(["heading"])
                .build(),
        );
        pace_section.append(
            &gtk::Label::builder()
                .label(
                    "Best pace for each distance across all synced running activities. \
                     Yellow = all-time best · Blue = last 30 days.",
                )
                .css_classes(["dim-label"])
                .halign(gtk::Align::Start)
                .wrap(true)
                .build(),
        );
        pace_section.append(&pace_chart);
        pace_section.append(&pace_x_row);
        pace_section.append(&pace_val_row);
        pace_section.append(&pace_legend);
        inner.append(&pace_section);

        // ── Volume ────────────────────────────────────────────────────────────
        let volume_section = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .visible(false)
            .build();

        volume_section.append(
            &gtk::Label::builder()
                .label("Volume")
                .halign(gtk::Align::Start)
                .css_classes(["heading"])
                .build(),
        );
        volume_section.append(
            &gtk::Label::builder()
                .label(
                    "Kilojoules measure total mechanical work done — a more accurate proxy \
                     for training load than time alone.",
                )
                .css_classes(["dim-label"])
                .halign(gtk::Align::Start)
                .wrap(true)
                .build(),
        );

        let volume_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .homogeneous(true)
            .build();

        let (wkj_frame, week_kj_label, _) = Self::make_metric_card("Week · kJ", "Kilojoules");
        let (whrs_frame, week_hrs_label, _) =
            Self::make_metric_card("Week · hours", "Training time");
        let (mkj_frame, month_kj_label, _) = Self::make_metric_card("This month", "Kilojoules");
        let (tot_frame, total_sessions_label, _) =
            Self::make_metric_card("Total", "Sessions recorded");
        volume_row.append(&wkj_frame);
        volume_row.append(&whrs_frame);
        volume_row.append(&mkj_frame);
        volume_row.append(&tot_frame);
        volume_section.append(&volume_row);
        inner.append(&volume_section);

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
                .label("Zone Distribution (All Time)")
                .halign(gtk::Align::Start)
                .css_classes(["heading"])
                .build(),
        );
        zone_section.append(
            &gtk::Label::builder()
                .label(
                    "Time spent in each power zone. Endurance athletes typically aim for \
                     70–80 % in Z1–Z2 (polarised) or Z2–Z3 (pyramidal).",
                )
                .css_classes(["dim-label"])
                .halign(gtk::Align::Start)
                .wrap(true)
                .build(),
        );
        zone_section.append(&zone_bar);
        zone_section.append(&zone_legend);
        inner.append(&zone_section);

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
                    let (r, g, b) = HR_ZONE_COLORS[i];
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
                .label("Heart Rate Zones (In-App Sessions)")
                .halign(gtk::Align::Start)
                .css_classes(["heading"])
                .build(),
        );
        hr_zone_section.append(
            &gtk::Label::builder()
                .label(
                    "Time in each HR zone based on your recorded max HR. \
                     Computed from in-app sessions only.",
                )
                .css_classes(["dim-label"])
                .halign(gtk::Align::Start)
                .wrap(true)
                .build(),
        );
        hr_zone_section.append(&hr_zone_bar);
        hr_zone_section.append(&hr_zone_legend);
        inner.append(&hr_zone_section);

        // ── Retrospective Analysis ────────────────────────────────────────────
        inner.append(
            &gtk::Label::builder()
                .label("Training Retrospective")
                .halign(gtk::Align::Start)
                .css_classes(["heading"])
                .build(),
        );
        inner.append(
            &gtk::Label::builder()
                .label(
                    "AI chain-of-thought analysis of strain, recovery patterns, and \
                     performance trends over the past week or month.",
                )
                .css_classes(["dim-label"])
                .halign(gtk::Align::Start)
                .wrap(true)
                .build(),
        );

        // Weekly retrospective card
        let (weekly_card, weekly_content, weekly_label, weekly_spinner, weekly_btn) =
            build_retro_card("Weekly Retrospective", "Analyse the past 7 days");
        inner.append(&weekly_card);

        // Monthly retrospective card
        let (monthly_card, monthly_content, monthly_label, monthly_spinner, monthly_btn) =
            build_retro_card("Monthly Retrospective", "Analyse the past 30 days");
        inner.append(&monthly_card);

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
                // API key pre-flight check
                let has_api_key = keystore::get_secret(keystore::KEY_ANTHROPIC)
                    .unwrap_or(None)
                    .map(|k| !k.trim().is_empty())
                    .unwrap_or(false);
                api_banner_r.set_revealed(!has_api_key);

                let records = rt_handle
                    .block_on(db::load_session_records(&pool))
                    .unwrap_or_default();
                let intervals_pairs = rt_handle
                    .block_on(db::load_intervals_tss_pairs(&pool))
                    .unwrap_or_default();
                let icu_activities = rt_handle
                    .block_on(db::load_intervals_activities(&pool))
                    .unwrap_or_default();
                let wellness = rt_handle
                    .block_on(db::load_wellness_recent(&pool, 14))
                    .unwrap_or_default();

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

                let (ctl, atl, _) =
                    compute_load_metrics(&records, &intervals_pairs, ftp_val, today);
                let tsb = ctl - atl;

                // PMC 90-day series
                let pmc_series = compute_pmc_series(&records, &intervals_pairs, ftp_val, today);
                let has_pmc = pmc_series.len() >= 2;
                pmc_section_r.set_visible(has_pmc);
                if has_pmc {
                    *pmc_data_r.borrow_mut() = pmc_series;
                    pmc_chart_r.queue_draw();
                }

                ctl_label.set_label(&format!("{:.0}", ctl));
                atl_label.set_label(&format!("{:.0}", atl));
                tsb_label.set_label(&format!("{:+.0}", tsb));

                // Per-card status descriptions
                ctl_status.set_label(ctl_status_text(ctl));
                ctl_status.set_visible(ctl > 0.5);
                atl_status.set_label(atl_status_text(atl));
                atl_status.set_visible(atl > 0.5);
                tsb_status.set_label(tsb_status_text(tsb));
                tsb_status.set_visible(ctl > 0.5 || atl > 0.5);

                // TSB value colour
                tsb_label.remove_css_class("success");
                tsb_label.remove_css_class("warning");
                if tsb > 5.0 {
                    tsb_label.add_css_class("success");
                } else if tsb < -10.0 {
                    tsb_label.add_css_class("warning");
                }

                // Plain-language form summary sentence
                if ctl > 0.5 || atl > 0.5 {
                    form_summary.set_label(&form_summary_text(ctl, tsb));
                    form_summary.set_visible(true);
                } else {
                    form_summary.set_visible(false);
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
                    let rhr_series =
                        build_14day_series(&wellness, today, |e| e.resting_hr.map(|v| v as f32));
                    let sleep_series = build_14day_series(&wellness, today, |e| {
                        e.sleep_secs.map(|s| s as f32 / 3600.0)
                    });
                    let score_series =
                        build_14day_series(&wellness, today, |e| e.sleep_score.map(|v| v as f32));
                    let steps_series =
                        build_14day_series(&wellness, today, |e| e.steps.map(|v| v as f32));
                    let cal_series =
                        build_14day_series(&wellness, today, |e| e.calories.map(|v| v as f32));

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
                        .and_then(|w| act.duration_secs.map(|d| w as f32 * d as f32 / 1000.0))
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
                total_sessions_label.set_label(&(records.len() + icu_activities.len()).to_string());

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

                // Zone distribution
                let mut zone_secs = [0u32; 7];
                for record in &records {
                    for dp in &record.session.data_points {
                        if let Some(watts) = dp.power_watts {
                            zone_secs[power_zone_index(watts, ftp_val)] += 1;
                        }
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
                let run_streams = rt_handle
                    .block_on(db::load_run_activity_streams(&pool))
                    .unwrap_or_default();
                let pace_curve = compute_pace_curve(&run_streams, cutoff_30d);
                let has_pace = pace_curve.iter().any(|&(a, _)| a > 0);
                pace_section_r.set_visible(has_pace);
                if has_pace {
                    for (lbl, &(all_p, _)) in pace_val_labels_r.iter().zip(pace_curve.iter()) {
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

                // Restore cached AI fitness insight if present
                let cached_insight = rt_handle
                    .block_on(db::get_setting(&pool, "ai.fitness_insight"))
                    .unwrap_or(None)
                    .unwrap_or_default();
                if !cached_insight.trim().is_empty() {
                    populate_ai_content(&ai_content_r, &analyse_label_r, &cached_insight);
                }
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

                let records = rt_a
                    .block_on(db::load_session_records(&pool_a))
                    .unwrap_or_default();
                let intervals_pairs = rt_a
                    .block_on(db::load_intervals_tss_pairs(&pool_a))
                    .unwrap_or_default();
                let icu_count = rt_a
                    .block_on(db::count_intervals_activities(&pool_a))
                    .unwrap_or(0) as usize;
                let wellness_raw = rt_a
                    .block_on(db::load_wellness_recent(&pool_a, 7))
                    .unwrap_or_default();
                let ftp_val = ftp_a.get();
                let today = Local::now().date_naive();
                let (ctl, atl, ctl_4wk_ago) =
                    compute_load_metrics(&records, &intervals_pairs, ftp_val, today);
                let tsb = ctl - atl;

                let week_tss = tss_week_a.borrow().clone();
                let athlete = athlete_a.borrow().clone();

                let athlete_context = rt_a
                    .block_on(db::get_setting(&pool_a, "coaching.athlete_context"))
                    .unwrap_or(None)
                    .unwrap_or_default();

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

                let btn_c = btn.clone();
                btn.set_sensitive(false);
                spinner_a.set_visible(true);
                spinner_a.start();
                label_a.set_text("Asking the AI Coach to analyse your fitness metrics…");
                label_a.remove_css_class("dim-label");

                let (tx, rx) = async_channel::bounded::<Result<String, String>>(1);
                rt_a.spawn(async move {
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

        // ── Retrospective Generate buttons ────────────────────────────────────
        for (period, btn, content, label, spinner, cache_key) in [
            (
                RetroPeriod::Weekly,
                &weekly_btn,
                &weekly_content,
                &weekly_label,
                &weekly_spinner,
                "ai.weekly_retrospective",
            ),
            (
                RetroPeriod::Monthly,
                &monthly_btn,
                &monthly_content,
                &monthly_label,
                &monthly_spinner,
                "ai.monthly_retrospective",
            ),
        ] {
            // Restore cached retrospective
            {
                let cached = rt_handle
                    .block_on(db::get_setting(&pool, cache_key))
                    .unwrap_or(None)
                    .unwrap_or_default();
                if !cached.is_empty() {
                    populate_ai_content(content, label, &cached);
                }
            }

            let pool_r = pool.clone();
            let rt_r = rt_handle.clone();
            let athlete_r = Rc::clone(&athlete);
            let ftp_r = Rc::clone(&ftp);
            let content_r = content.clone();
            let label_r = label.clone();
            let spinner_r = spinner.clone();
            let cache_key_s = cache_key.to_string();

            btn.connect_clicked(move |btn| {
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

                let records = rt_r
                    .block_on(db::load_sessions_between(&pool_r, &start_utc, &end_utc))
                    .unwrap_or_default();
                let icu_acts = rt_r
                    .block_on(db::load_intervals_activities(&pool_r))
                    .unwrap_or_default();
                let intervals_all = rt_r
                    .block_on(db::load_intervals_tss_pairs(&pool_r))
                    .unwrap_or_default();
                let wellness_raw = rt_r
                    .block_on(db::load_wellness_between(
                        &pool_r,
                        &start_date.format("%Y-%m-%d").to_string(),
                        &today.format("%Y-%m-%d").to_string(),
                    ))
                    .unwrap_or_default();
                let all_records = rt_r
                    .block_on(db::load_session_records(&pool_r))
                    .unwrap_or_default();
                let athlete_context = rt_r
                    .block_on(db::get_setting(&pool_r, "coaching.athlete_context"))
                    .unwrap_or(None)
                    .unwrap_or_default();

                let ftp_val = ftp_r.get();
                let (ctl_end, atl_end, _) =
                    compute_load_metrics(&all_records, &intervals_all, ftp_val, today);
                let ctl_start_date = today - Duration::days(period_days);
                let (ctl_start, _, _) =
                    compute_load_metrics(&all_records, &intervals_all, ftp_val, ctl_start_date);
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
                    athlete: athlete_r.borrow().clone(),
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

                btn.set_sensitive(false);
                spinner_r.set_visible(true);
                spinner_r.start();
                label_r.set_text(&format!(
                    "Generating {} retrospective analysis…",
                    period.label()
                ));
                label_r.remove_css_class("dim-label");

                let (tx, rx) = async_channel::bounded::<Result<String, String>>(1);
                rt_r.spawn(async move {
                    let r = get_suggestion(&api_key, &prompt, 1500)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx.send(r).await;
                });

                let label_c = label_r.clone();
                let content_c = content_r.clone();
                let spinner_c = spinner_r.clone();
                let btn_c = btn.clone();
                let pool_c = pool_r.clone();
                let rt_c = rt_r.clone();
                let key_c = cache_key_s.clone();
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
            });
        }

        reload();

        (Self { root }, reload)
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    fn make_metric_card(title: &str, subtitle: &str) -> (gtk::Box, gtk::Label, gtk::Label) {
        let card = gtk::Box::builder()
            .css_classes(["card"])
            .hexpand(true)
            .build();

        let vbox = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
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

        let value = gtk::Label::builder()
            .label("—")
            .halign(gtk::Align::Start)
            .css_classes(["title-2", "numeric"])
            .build();
        vbox.append(&value);

        vbox.append(
            &gtk::Label::builder()
                .label(subtitle)
                .halign(gtk::Align::Start)
                .css_classes(["caption", "dim-label"])
                .build(),
        );

        let status = gtk::Label::builder()
            .label("")
            .halign(gtk::Align::Start)
            .css_classes(["caption"])
            .visible(false)
            .build();
        vbox.append(&status);

        card.append(&vbox);
        (card, value, status)
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

        // Sparkline
        let data: Rc<RefCell<Vec<f32>>> = Rc::new(RefCell::new(vec![]));
        let chart = gtk::DrawingArea::builder()
            .content_height(42)
            .hexpand(true)
            .build();
        let data_ref = Rc::clone(&data);
        chart.set_draw_func(move |_w, cr, width, height| {
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
            cr.set_source_rgba(0.47, 0.68, 0.93, 0.12);
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
            cr.set_source_rgba(0.47, 0.68, 0.93, 0.85);
            cr.set_line_width(2.0);
            cr.move_to(px(fi), py(fv));
            for &(i, v) in &points[1..] {
                cr.line_to(px(i), py(v));
            }
            cr.stroke().ok();

            // Dot at latest point
            let (ldi, ldv) = *points.last().expect("len >= 2");
            cr.set_source_rgba(0.47, 0.68, 0.93, 1.0);
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

/// Build a retrospective analysis card.
/// Returns `(card, ai_content_box, fallback_label, spinner, generate_btn)`.
fn build_retro_card(
    title: &str,
    btn_label: &str,
) -> (gtk::Box, gtk::Box, gtk::Label, gtk::Spinner, gtk::Button) {
    let card = gtk::Box::builder()
        .css_classes(["card"])
        .orientation(gtk::Orientation::Vertical)
        .build();

    let header = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_top(10)
        .margin_bottom(6)
        .margin_start(12)
        .margin_end(12)
        .build();

    header.append(
        &gtk::Label::builder()
            .label(title)
            .halign(gtk::Align::Start)
            .hexpand(true)
            .css_classes(["heading"])
            .build(),
    );

    let spinner = gtk::Spinner::new();
    spinner.set_visible(false);
    header.append(&spinner);

    let btn = gtk::Button::builder()
        .label(btn_label)
        .css_classes(["pill"])
        .tooltip_text("Generate AI retrospective analysis")
        .valign(gtk::Align::Center)
        .build();
    header.append(&btn);

    card.append(&header);
    card.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(0)
        .build();

    let label = gtk::Label::builder()
        .label("Select the button above to generate a retrospective analysis.")
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
    card.append(&content);

    (card, content, label, spinner, btn)
}
