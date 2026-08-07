//! The performance-management model: Fitness (CTL), Fatigue (ATL) and Form (TSB).
//!
//! Every ride the app knows about contributes a Training Stress Score to the day
//! it was ridden on. Those daily totals are run through two exponential moving
//! averages — a 42-day one that responds slowly and stands for accumulated
//! fitness, and a 7-day one that responds quickly and stands for fatigue. Form
//! is what is left when fatigue is taken off fitness.
//!
//! Rides that Intervals.icu already accounts for are skipped here: their TSS
//! arrives through `intervals_pairs`, and counting them twice would inflate both
//! averages.
//!
//! Both averages are warmed up from the earliest day with any data rather than
//! from the start of the display window, so a 90-day chart does not open at
//! zero for a rider with two years of history.

use std::collections::HashMap;

use chrono::{Duration, Local, NaiveDate};

use crate::data::db::SessionSummary;

/// Time constant of the Fitness average, in days.
const CTL_TIME_CONSTANT_DAYS: f64 = 42.0;
/// Time constant of the Fatigue average, in days.
const ATL_TIME_CONSTANT_DAYS: f64 = 7.0;

/// Fitness, Fatigue, and the Fitness figure from four weeks earlier.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LoadMetrics {
    /// Chronic Training Load — "Fitness".
    pub ctl: f64,
    /// Acute Training Load — "Fatigue".
    pub atl: f64,
    /// CTL as it stood four weeks before `today`, for the fitness trend arrow.
    pub ctl_4wk_ago: f64,
}

impl LoadMetrics {
    /// Training Stress Balance — "Form". Fitness minus fatigue.
    pub fn tsb(&self) -> f64 {
        self.ctl - self.atl
    }
}

/// One day of the performance-management chart.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PmcPoint {
    pub date: NaiveDate,
    pub ctl: f64,
    pub atl: f64,
    pub tsb: f64,
}

/// Total the TSS of every ride onto the local calendar day it was ridden on.
///
/// `fallback_ftp` scores only those rides that carry no stamped FTP of their own.
fn daily_tss(
    rides: &[SessionSummary],
    intervals_pairs: &[(NaiveDate, f32)],
    fallback_ftp: u32,
) -> HashMap<NaiveDate, f32> {
    let mut totals: HashMap<NaiveDate, f32> = HashMap::new();
    for ride in rides {
        if ride.counted_via_intervals() {
            continue;
        }
        let date = ride.started_at.with_timezone(&Local).date_naive();
        if let Some(tss) = ride.tss(fallback_ftp) {
            *totals.entry(date).or_insert(0.0) += tss;
        }
    }
    for &(date, tss) in intervals_pairs {
        *totals.entry(date).or_insert(0.0) += tss;
    }
    totals
}

/// Smoothing factor for an EMA with the given time constant in days.
fn alpha(time_constant_days: f64) -> f64 {
    1.0 - (-1.0 / time_constant_days).exp()
}

/// Walk one day at a time from the earliest day with data up to `today`,
/// advancing both averages, and hand each day to `visit`.
///
/// Days with no riding still get a step — that decay is the whole point of the
/// model — so this cannot iterate the sparse map directly.
fn walk_days(
    totals: &HashMap<NaiveDate, f32>,
    today: NaiveDate,
    mut visit: impl FnMut(NaiveDate, f64, f64),
) {
    let Some(earliest) = totals.keys().min().copied() else {
        return;
    };
    let (ctl_alpha, atl_alpha) = (alpha(CTL_TIME_CONSTANT_DAYS), alpha(ATL_TIME_CONSTANT_DAYS));
    let (mut ctl, mut atl) = (0.0_f64, 0.0_f64);
    let mut date = earliest;
    loop {
        let tss = totals.get(&date).copied().unwrap_or(0.0) as f64;
        ctl += ctl_alpha * (tss - ctl);
        atl += atl_alpha * (tss - atl);
        visit(date, ctl, atl);
        if date == today {
            break;
        }
        match date.succ_opt() {
            Some(next) => date = next,
            // Only reachable at the end of the representable calendar.
            None => break,
        }
    }
}

/// Fitness, Fatigue and the four-weeks-ago Fitness figure, as of `today`.
///
/// Returns all zeroes when there is nothing to average.
pub fn compute_load_metrics(
    rides: &[SessionSummary],
    intervals_pairs: &[(NaiveDate, f32)],
    fallback_ftp: u32,
    today: NaiveDate,
) -> LoadMetrics {
    let totals = daily_tss(rides, intervals_pairs, fallback_ftp);
    let four_wk_ago = today - Duration::weeks(4);
    let mut metrics = LoadMetrics::default();
    walk_days(&totals, today, |date, ctl, atl| {
        if date == four_wk_ago {
            metrics.ctl_4wk_ago = ctl;
        }
        metrics.ctl = ctl;
        metrics.atl = atl;
    });
    metrics
}

/// The performance-management chart: one point per day for the `window_days`
/// leading up to `today`, with both averages warmed up from all prior history.
pub fn compute_pmc_series(
    rides: &[SessionSummary],
    intervals_pairs: &[(NaiveDate, f32)],
    fallback_ftp: u32,
    today: NaiveDate,
    window_days: i64,
) -> Vec<PmcPoint> {
    let totals = daily_tss(rides, intervals_pairs, fallback_ftp);
    let window_start = today - Duration::days(window_days);
    let mut series = Vec::new();
    walk_days(&totals, today, |date, ctl, atl| {
        if date >= window_start {
            series.push(PmcPoint {
                date,
                ctl,
                atl,
                tsb: ctl - atl,
            });
        }
    });
    series
}

/// How fresh the rider is, as a band rather than a raw number.
///
/// One set of thresholds for the whole app: the Fitness page's hero, the
/// coaching prompt and the morning briefing all read the same number, so they
/// have to agree on what counts as fresh. They previously each carried their
/// own table and disagreed — at a Form of +3 one screen said "normal training
/// fatigue" while another told the coach the rider was in good form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TsbBand {
    VeryFresh,
    Fresh,
    Normal,
    Elevated,
    High,
}

impl TsbBand {
    /// Classify a Form value. Boundaries are exclusive at the top of each band,
    /// so a Form of exactly 25 is `Fresh` and 25.1 is `VeryFresh`.
    pub fn of(tsb: f64) -> Self {
        if tsb > 25.0 {
            Self::VeryFresh
        } else if tsb > 5.0 {
            Self::Fresh
        } else if tsb > -10.0 {
            Self::Normal
        } else if tsb > -30.0 {
            Self::Elevated
        } else {
            Self::High
        }
    }

    /// Headline phrase for the Fitness page — addressed to the rider.
    pub fn status_text(&self) -> &'static str {
        match self {
            Self::VeryFresh => "Very fresh — consider adding volume",
            Self::Fresh => "Fresh — ready for quality work",
            Self::Normal => "Normal training fatigue",
            Self::Elevated => "Elevated fatigue — consider easier days",
            Self::High => "High fatigue — prioritise rest",
        }
    }

    /// One word for a chip or badge, where the full phrase will not fit.
    ///
    /// Collapses the five bands to the three distinctions a workout
    /// recommendation acts on.
    pub fn short_label(&self) -> &'static str {
        match self {
            Self::VeryFresh | Self::Fresh => "Fresh",
            Self::Normal => "Normal",
            Self::Elevated | Self::High => "Fatigued",
        }
    }

    /// Whether form is low enough that recovery should outrank the rider's
    /// stated goals when recommending a session.
    pub fn is_fatigued(&self) -> bool {
        matches!(self, Self::Elevated | Self::High)
    }

    /// Whether the rider is rested enough that hard, quality work is a good
    /// call — the counterpart to [`Self::is_fatigued`].
    pub fn is_fresh(&self) -> bool {
        matches!(self, Self::VeryFresh | Self::Fresh)
    }

    /// The same reading, phrased for an AI prompt — describing the rider in the
    /// third person and saying what it implies for the session being planned.
    pub fn prompt_description(&self) -> &'static str {
        match self {
            Self::VeryFresh => "very fresh (risk of detraining if sustained)",
            Self::Fresh => "good form — ready for quality work",
            Self::Normal => "normal training fatigue",
            Self::Elevated => "elevated fatigue — consider modifying the session",
            Self::High => "significant fatigue — recovery priority",
        }
    }
}

/// Convenience for the common "what does this Form value read as" call.
pub fn tsb_status_text(tsb: f64) -> &'static str {
    TsbBand::of(tsb).status_text()
}

#[cfg(test)]
mod test_support {
    use super::*;
    use chrono::TimeZone;

    /// Build a summary standing in for a ride of `tss` TSS on `date`.
    pub(super) fn summary_on(date: NaiveDate, tss: f32) -> SessionSummary {
        // TSS = (NP/FTP)^2 * hours * 100. Fix FTP and duration at an hour, and the
        // NP that produces the wanted TSS falls out.
        let ftp = 250.0_f32;
        let np = ftp * (tss / 100.0).sqrt();
        SessionSummary {
            id: 0,
            started_at: Local
                .from_local_datetime(&date.and_hms_opt(12, 0, 0).expect("noon is a valid time"))
                .single()
                .expect("noon is unambiguous in every timezone the tests run in")
                .into(),
            duration_secs: 3600,
            normalised_power: Some(np),
            average_power: Some(np),
            kilojoules: 0.0,
            ftp_watts: Some(ftp as u32),
            rpe: None,
            workout_name: None,
            uploaded_to_icu: false,
            icu_id: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::summary_on;
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("hardcoded valid date")
    }

    #[test]
    fn should_return_zero_metrics_when_there_are_no_rides() {
        let m = compute_load_metrics(&[], &[], 250, date(2026, 8, 7));
        assert_eq!(m, LoadMetrics::default());
        assert_eq!(m.tsb(), 0.0);
    }

    #[test]
    fn should_move_fatigue_faster_than_fitness_after_a_single_ride() {
        let day = date(2026, 8, 1);
        let rides = [summary_on(day, 100.0)];
        let m = compute_load_metrics(&rides, &[], 250, day);
        // One 100 TSS day: ATL takes 1/7th of it, CTL only 1/42nd.
        assert!(m.atl > m.ctl, "fatigue should outrun fitness on day one");
        assert!(m.tsb() < 0.0, "a hard day leaves form negative");
    }

    #[test]
    fn should_decay_both_averages_on_days_with_no_riding() {
        let ride_day = date(2026, 8, 1);
        let rides = [summary_on(ride_day, 100.0)];
        let after_ride = compute_load_metrics(&rides, &[], 250, ride_day);
        // Three weeks: after a single ride the two averages cross at about day
        // 14.5, so a fortnight is too early to assert the sign of form.
        let later = compute_load_metrics(&rides, &[], 250, ride_day + Duration::days(21));

        assert!(later.atl < after_ride.atl);
        assert!(later.ctl < after_ride.ctl);
        // Fatigue sheds much faster than fitness, so form turns positive.
        assert!(later.tsb() > 0.0, "form recovers during rest");
    }

    #[test]
    fn should_not_count_a_ride_intervals_icu_already_reported() {
        let day = date(2026, 8, 1);
        let mut ride = summary_on(day, 100.0);
        ride.uploaded_to_icu = true;

        // The same ride arriving through the Intervals.icu pairs instead.
        let via_icu = compute_load_metrics(&[ride], &[(day, 100.0)], 250, day);
        let direct = compute_load_metrics(&[summary_on(day, 100.0)], &[], 250, day);

        assert!(
            (via_icu.ctl - direct.ctl).abs() < 1e-9,
            "a synced ride must count once, not twice"
        );
    }

    #[test]
    fn should_sum_two_rides_on_the_same_day() {
        let day = date(2026, 8, 1);
        let two = compute_load_metrics(
            &[summary_on(day, 50.0), summary_on(day, 50.0)],
            &[],
            250,
            day,
        );
        let one = compute_load_metrics(&[summary_on(day, 100.0)], &[], 250, day);
        assert!((two.ctl - one.ctl).abs() < 0.5, "a double day totals up");
    }

    #[test]
    fn should_report_fitness_from_four_weeks_earlier() {
        let start = date(2026, 7, 1);
        // Ride every day for six weeks, so CTL is still climbing at the end.
        let rides: Vec<_> = (0..42)
            .map(|i| summary_on(start + Duration::days(i), 60.0))
            .collect();
        let today = start + Duration::days(41);
        let m = compute_load_metrics(&rides, &[], 250, today);
        assert!(
            m.ctl_4wk_ago > 0.0 && m.ctl_4wk_ago < m.ctl,
            "fitness four weeks ago should be lower but not zero"
        );
    }

    #[test]
    fn should_warm_the_series_up_from_history_before_the_window() {
        let start = date(2026, 1, 1);
        let rides: Vec<_> = (0..200)
            .map(|i| summary_on(start + Duration::days(i), 60.0))
            .collect();
        let today = start + Duration::days(199);
        let series = compute_pmc_series(&rides, &[], 250, today, 90);

        assert_eq!(series.len(), 91, "90-day window is inclusive of both ends");
        assert!(
            series[0].ctl > 40.0,
            "the chart should open at the fitness already built, not at zero"
        );
        assert_eq!(series.last().expect("non-empty").date, today);
    }

    #[test]
    fn should_agree_between_the_series_and_the_point_metrics() {
        let start = date(2026, 6, 1);
        let rides: Vec<_> = (0..60)
            .filter(|i| i % 3 != 0) // rest every third day
            .map(|i| summary_on(start + Duration::days(i), 80.0))
            .collect();
        let today = start + Duration::days(59);

        let m = compute_load_metrics(&rides, &[], 250, today);
        let last = *compute_pmc_series(&rides, &[], 250, today, 90)
            .last()
            .expect("series should reach today");

        assert!((last.ctl - m.ctl).abs() < 1e-9);
        assert!((last.atl - m.atl).abs() < 1e-9);
        assert!((last.tsb - m.tsb()).abs() < 1e-9);
    }

    // ── TSB bands ────────────────────────────────────────────────────────────
    // Boundaries are where the copies used to disagree, so pin each one.

    #[test]
    fn should_classify_tsb_at_every_band_boundary() {
        assert_eq!(TsbBand::of(25.1), TsbBand::VeryFresh);
        assert_eq!(TsbBand::of(25.0), TsbBand::Fresh);
        assert_eq!(TsbBand::of(5.1), TsbBand::Fresh);
        assert_eq!(TsbBand::of(5.0), TsbBand::Normal);
        assert_eq!(TsbBand::of(0.0), TsbBand::Normal);
        assert_eq!(TsbBand::of(-9.9), TsbBand::Normal);
        assert_eq!(TsbBand::of(-10.0), TsbBand::Elevated);
        assert_eq!(TsbBand::of(-29.9), TsbBand::Elevated);
        assert_eq!(TsbBand::of(-30.0), TsbBand::High);
    }

    #[test]
    fn should_read_a_slightly_positive_form_as_normal_everywhere() {
        // The case the old tables split on: the briefing called +3 "good form"
        // while the fitness page called it normal fatigue.
        let band = TsbBand::of(3.0);
        assert_eq!(band, TsbBand::Normal);
        assert_eq!(band.status_text(), "Normal training fatigue");
        assert_eq!(band.prompt_description(), "normal training fatigue");
    }

    #[test]
    fn should_read_minus_25_as_elevated_not_severe() {
        // The briefing used to escalate this to "rest is recommended".
        assert_eq!(TsbBand::of(-25.0), TsbBand::Elevated);
    }

    #[test]
    fn should_collapse_to_three_labels_for_chips() {
        assert_eq!(TsbBand::VeryFresh.short_label(), "Fresh");
        assert_eq!(TsbBand::Fresh.short_label(), "Fresh");
        assert_eq!(TsbBand::Normal.short_label(), "Normal");
        assert_eq!(TsbBand::Elevated.short_label(), "Fatigued");
        assert_eq!(TsbBand::High.short_label(), "Fatigued");
    }

    #[test]
    fn should_treat_only_the_negative_bands_as_fatigued() {
        // The library used to call +10 "Normal" while the fitness page called
        // it "Fresh"; both now read the band.
        assert_eq!(TsbBand::of(10.0).short_label(), "Fresh");
        assert!(!TsbBand::of(10.0).is_fatigued());
        assert!(!TsbBand::of(-9.9).is_fatigued());
        assert!(TsbBand::of(-10.0).is_fatigued());
        assert!(TsbBand::of(-40.0).is_fatigued());
    }

    #[test]
    fn should_never_call_a_band_both_fresh_and_fatigued() {
        for tenths in -500..=500 {
            let tsb = tenths as f64 / 10.0;
            let band = TsbBand::of(tsb);
            assert!(
                !(band.is_fresh() && band.is_fatigued()),
                "form {tsb} classified as both"
            );
            // Normal is the only band that is neither.
            assert_eq!(
                band == TsbBand::Normal,
                !band.is_fresh() && !band.is_fatigued(),
                "form {tsb} fell between the labels"
            );
        }
    }

    #[test]
    fn should_expose_the_same_band_through_the_status_text_helper() {
        assert_eq!(tsb_status_text(30.0), TsbBand::VeryFresh.status_text());
        assert_eq!(tsb_status_text(-40.0), TsbBand::High.status_text());
    }
}
