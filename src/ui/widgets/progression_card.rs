//! "Compared with Last Time" — where a ride stands against earlier attempts at
//! the same named effort.
//!
//! Shown after a ride finishes and when looking back at one. The card decides
//! for itself whether it has anything worth saying: with no earlier attempt it
//! renders nothing at all rather than an empty state, because a first ride is
//! not a progression.
//!
//! The comparison itself is worked out in [`crate::training::progression`];
//! this module only draws it.

use adw::prelude::*;
use chrono::NaiveDate;
use sqlx::SqlitePool;

use crate::data::db;
use crate::training::engine::WorkoutEngine;
use crate::training::progression::{build_history, compare, prior_efforts, Comparison, Effort};
use crate::ui::widgets::sparkline::Sparkline;

/// A change smaller than this fraction is noise from a day's form, not a
/// direction of travel, so it is shown as a level result rather than coloured.
const NOISE_THRESHOLD: f32 = 0.01;

/// Load the effort history and fill `holder` with the card, if there is one.
///
/// `holder` is emptied first and left hidden when the ride has no earlier
/// attempt, so callers can hand over the same box on every open without
/// clearing it themselves.
///
/// The two table reads happen on the tokio runtime; the widgets are built in
/// the callback, which GLib runs on the main thread — see CLAUDE.md §2.3.
pub fn attach(
    holder: &gtk::Box,
    current: Effort,
    pool: SqlitePool,
    rt_handle: &tokio::runtime::Handle,
) {
    while let Some(child) = holder.first_child() {
        holder.remove(&child);
    }
    holder.set_visible(false);

    let holder = holder.clone();
    crate::ui::spawn_to_main(
        rt_handle,
        async move {
            let sessions = db::load_session_summaries(&pool).await?;
            let activities = db::load_intervals_activities(&pool).await?;
            anyhow::Ok((sessions, activities))
        },
        move |loaded| {
            let (sessions, activities) = match loaded {
                Ok(v) => v,
                Err(e) => {
                    // The ride's own figures are all still on screen; the
                    // comparison is the only thing missing.
                    tracing::error!("Could not load ride history for comparison: {e}");
                    return;
                }
            };

            let history = build_history(&sessions, &activities);
            let priors = prior_efforts(&history, &current.name, current.date);
            let Some(comparison) = compare(&current, &priors) else {
                return;
            };

            holder.append(&build(&current, &comparison));
            holder.set_visible(true);
        },
    );
}

/// Draw the card for a ride that does have earlier attempts.
fn build(current: &Effort, comparison: &Comparison) -> gtk::Box {
    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build();

    let group = adw::PreferencesGroup::builder()
        .title("Compared with Last Time")
        .description(format!(
            "{} · first ridden {}",
            ordinal_effort(comparison.attempt),
            format_date(comparison.since)
        ))
        .build();

    let previous = &comparison.previous;
    let when = format_date(previous.date);

    // ── Power ─────────────────────────────────────────────────────────────────
    if let (Some(now), Some(then)) = (current.power(), previous.power()) {
        // Naming the figure honestly: a ride imported without a normalised
        // power is being compared on its mean, and saying otherwise would
        // overstate what the number is.
        let title = if current.normalised_power.is_some() && previous.normalised_power.is_some() {
            "Normalised Power"
        } else {
            "Average Power"
        };
        // Power is the one figure compared across indoor and outdoor rides, so
        // the earlier ride says which it was when that differs from this one.
        let row = metric_row(
            title,
            &format!("{now} W"),
            &format!("was {then} W on {when}{}", setting_note(current, previous)),
            now as f32 - then as f32,
            then as f32,
            true,
            |d| format!("{d:+.0} W"),
        );
        if comparison.is_best() {
            row.add_suffix(&badge("Best yet"));
        }
        group.add(&row);
    }

    // ── Aerobic efficiency ────────────────────────────────────────────────────
    // The same watts at a lower heart rate is the fitness signal the rider
    // cannot see from either number on its own. Compared within one setting
    // only — see `Comparison::efficiency_baseline`.
    if let Some(baseline) = comparison.efficiency_baseline(current) {
        let now = current.efficiency().unwrap_or_default();
        let then = baseline.efficiency().unwrap_or_default();
        group.add(&metric_row(
            "Aerobic Efficiency",
            &format!("{now:.2} W/bpm"),
            &format!("was {then:.2} W/bpm on {}", format_date(baseline.date)),
            now - then,
            then,
            true,
            |d| format!("{d:+.2}"),
        ));
    }

    // ── Elapsed time, only over the same ground ───────────────────────────────
    // Only against a ride over close to the same distance and in the same
    // setting — see `Comparison::time_baseline`.
    if let Some(baseline) = comparison.time_baseline(current) {
        let now = current.duration_secs as f32;
        let then = baseline.duration_secs as f32;
        group.add(&metric_row(
            "Elapsed Time",
            &WorkoutEngine::format_duration(current.duration_secs),
            &format!(
                "was {} on {}",
                WorkoutEngine::format_duration(baseline.duration_secs),
                format_date(baseline.date)
            ),
            now - then,
            then,
            // Faster is better, so a negative change is the good direction.
            false,
            |d| {
                let sign = if d < 0.0 { "−" } else { "+" };
                format!("{sign}{}", WorkoutEngine::format_duration(d.abs() as u32))
            },
        ));
    }

    // ── Perceived effort ──────────────────────────────────────────────────────
    // Only ever recorded on rides taken in the app, so this row is absent for
    // anything that came in from Intervals.icu.
    if let (Some(now), Some(then)) = (current.rpe, previous.rpe) {
        group.add(&metric_row(
            "Perceived Effort",
            &format!("{now} / 10"),
            &format!("was {then} / 10 on {when}"),
            now as f32 - then as f32,
            then as f32,
            // Doing the same work while it feels easier is progress.
            false,
            |d| format!("{d:+.0}"),
        ));
    }

    root.append(&group);

    // ── The series behind the headline ────────────────────────────────────────
    if let Some(chart) = power_chart(current, comparison) {
        root.append(&chart);
    }

    root
}

/// A card holding the sparkline of power across every attempt.
///
/// `None` when fewer than two attempts carry a power reading — the sparkline
/// needs two points to draw a line, and an empty chart frame is worse than no
/// chart.
fn power_chart(current: &Effort, comparison: &Comparison) -> Option<gtk::Box> {
    let readings = comparison.power_series.iter().filter(|&&v| v > 0.0).count();
    if readings < 2 {
        return None;
    }

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
            .label(chart_caption(current.indoor, comparison.other_setting))
            .halign(gtk::Align::Start)
            .css_classes(["caption", "dim-label"])
            .build(),
    );

    let chart = Sparkline::new();
    chart.set_values(&comparison.power_series);
    chart.widget().set_accessible_role(gtk::AccessibleRole::Img);
    chart
        .widget()
        .update_property(&[gtk::accessible::Property::Label(&describe_series(
            &comparison.power_series,
        ))]);
    chart
        .widget()
        .set_tooltip_text(Some("Power for each attempt, oldest on the left"));
    vbox.append(chart.widget());

    card.append(&vbox);
    Some(card)
}

/// ", outdoors" or ", indoors" when `prior` was ridden in the other setting from
/// `current`, and nothing when they match — the rider knows where they just
/// rode, so only the exception needs saying.
fn setting_note(current: &Effort, prior: &Effort) -> &'static str {
    match (current.indoor, prior.indoor) {
        (true, false) => ", outdoors",
        (false, true) => ", indoors",
        _ => "",
    }
}

/// The sparkline's caption. The line mixes indoor and outdoor rides without
/// marking which point is which, so the caption counts the ones ridden in the
/// other setting from this ride.
fn chart_caption(indoor: bool, other_setting: usize) -> String {
    match other_setting {
        0 => "Power Across Every Effort".to_string(),
        n => format!(
            "Power Across Every Effort · {n} {} {}",
            if n == 1 { "ride" } else { "rides" },
            if indoor { "outdoors" } else { "indoors" }
        ),
    }
}

/// Spoken description of the sparkline, for a rider using a screen reader.
fn describe_series(series: &[f32]) -> String {
    let readings: Vec<u32> = series
        .iter()
        .filter(|&&v| v > 0.0)
        .map(|&v| v as u32)
        .collect();
    match (readings.first(), readings.last()) {
        (Some(first), Some(last)) => format!(
            "Power across {} efforts, from {} watts to {} watts",
            readings.len(),
            first,
            last
        ),
        _ => "Power across earlier efforts".to_string(),
    }
}

/// One comparison row: what it is now, what it was, and the change.
///
/// `delta` and `baseline` are given separately so the change can be judged
/// against the size of the figure — two watts on 240 is not a direction.
/// `higher_is_better` decides which way counts as progress; `format_delta`
/// renders it in the row's own units.
fn metric_row(
    title: &str,
    value: &str,
    subtitle: &str,
    delta: f32,
    baseline: f32,
    higher_is_better: bool,
    format_delta: impl Fn(f32) -> String,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .build();

    let value_label = gtk::Label::builder()
        .label(value)
        .css_classes(["numeric"])
        .valign(gtk::Align::Center)
        .build();
    row.add_suffix(&value_label);

    let significant = baseline.abs() > f32::EPSILON
        && (delta.abs() / baseline.abs()) > NOISE_THRESHOLD
        && delta != 0.0;
    let improved = if higher_is_better {
        delta > 0.0
    } else {
        delta < 0.0
    };

    let style = if !significant {
        "dim-label"
    } else if improved {
        "success"
    } else {
        "warning"
    };

    let delta_label = gtk::Label::builder()
        .label(if delta == 0.0 {
            "level".to_string()
        } else {
            format_delta(delta)
        })
        .css_classes(["numeric", "caption-heading", style])
        .valign(gtk::Align::Center)
        .build();
    row.add_suffix(&delta_label);

    row
}

/// The small "Best yet" marker on a record ride.
fn badge(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .css_classes(["caption-heading", "accent"])
        .valign(gtk::Align::Center)
        .build()
}

/// "4th effort", counting this ride.
fn ordinal_effort(attempt: usize) -> String {
    // 11th, 12th and 13th break the last-digit rule.
    let suffix = match (attempt % 10, attempt % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{attempt}{suffix} effort")
}

fn format_date(date: NaiveDate) -> String {
    date.format("%-d %B %Y").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_name_the_ordinal_effort() {
        assert_eq!(ordinal_effort(1), "1st effort");
        assert_eq!(ordinal_effort(2), "2nd effort");
        assert_eq!(ordinal_effort(3), "3rd effort");
        assert_eq!(ordinal_effort(4), "4th effort");
        assert_eq!(ordinal_effort(21), "21st effort");
    }

    #[test]
    fn should_use_th_for_the_teens() {
        // 11th, not 11st.
        assert_eq!(ordinal_effort(11), "11th effort");
        assert_eq!(ordinal_effort(12), "12th effort");
        assert_eq!(ordinal_effort(13), "13th effort");
        assert_eq!(ordinal_effort(111), "111th effort");
    }

    #[test]
    fn should_describe_the_series_for_a_screen_reader() {
        let described = describe_series(&[180.0, 0.0, 210.0, 230.0]);
        assert_eq!(
            described,
            "Power across 3 efforts, from 180 watts to 230 watts"
        );
    }

    #[test]
    fn should_describe_an_empty_series_without_panicking() {
        assert_eq!(describe_series(&[]), "Power across earlier efforts");
        assert_eq!(describe_series(&[0.0]), "Power across earlier efforts");
    }

    fn ride(indoor: bool) -> Effort {
        Effort {
            date: NaiveDate::from_ymd_opt(2026, 6, 1).expect("hardcoded valid date"),
            name: "Loop".into(),
            duration_secs: 3600,
            normalised_power: Some(200),
            average_power: Some(200),
            average_hr: None,
            distance_m: None,
            rpe: None,
            indoor,
        }
    }

    #[test]
    fn should_name_the_setting_only_when_it_differs() {
        assert_eq!(setting_note(&ride(true), &ride(false)), ", outdoors");
        assert_eq!(setting_note(&ride(false), &ride(true)), ", indoors");
        assert_eq!(setting_note(&ride(true), &ride(true)), "");
        assert_eq!(setting_note(&ride(false), &ride(false)), "");
    }

    #[test]
    fn should_leave_the_caption_plain_when_every_ride_shares_a_setting() {
        assert_eq!(chart_caption(true, 0), "Power Across Every Effort");
    }

    #[test]
    fn should_count_the_rides_from_the_other_setting_in_the_caption() {
        assert_eq!(
            chart_caption(true, 1),
            "Power Across Every Effort · 1 ride outdoors"
        );
        assert_eq!(
            chart_caption(false, 3),
            "Power Across Every Effort · 3 rides indoors"
        );
    }
}
