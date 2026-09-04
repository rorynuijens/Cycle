//! Pure analytics behind the Fitness page's charts.
//!
//! These reductions used to live inside the page that draws them, where nothing
//! could test them and nothing else could reuse them. Everything here takes
//! plain domain types and returns plain data — no GTK (CLAUDE.md §2.6).

use chrono::{Datelike, Duration, Local, NaiveDate};

use crate::data::db::{IntervalsActivity, SessionRecord, SessionSummary, WellnessEntry};
use crate::data::streams::ActivityStreams;

/// Rolling window lengths, in seconds, plotted on the mean-maximal power curve.
pub const CURVE_DURATIONS: [usize; 10] = [5, 10, 30, 60, 120, 300, 600, 1200, 1800, 3600];

/// Standard race distances used for the pace curve, in metres.
pub const PACE_DISTANCES: [f32; 8] = [
    400.0, 800.0, 1609.0, 3000.0, 5000.0, 10000.0, 21097.5, 42195.0,
];

/// Display labels for [`PACE_DISTANCES`], in the same order.
pub const PACE_LABELS: [&str; 8] = [
    "400 m", "800 m", "1 mi", "3 km", "5 km", "10 km", "Half", "Full",
];

/// Days of history behind each wellness sparkline.
pub const WELLNESS_WINDOW_DAYS: i64 = 14;

/// How far back "recent" bests look, in days — the second series on the power
/// and pace curves.
pub const RECENT_WINDOW_DAYS: i64 = 30;

/// The local calendar date a session was ridden on.
///
/// Sessions are stored in UTC but bucketed by the rider's own day: a 23:30
/// local ride belongs to that evening, not to the following UTC day.
pub fn session_date(record: &SessionRecord) -> NaiveDate {
    record.session.started_at.with_timezone(&Local).date_naive()
}

/// Monday of the week containing `today`.
pub fn week_start_of(today: NaiveDate) -> NaiveDate {
    today - Duration::days(today.weekday().num_days_from_monday() as i64)
}

/// The first of the month containing `today`.
pub fn month_start_of(today: NaiveDate) -> NaiveDate {
    NaiveDate::from_ymd_opt(today.year(), today.month(), 1)
        .expect("day 1 of any calendar month is always valid")
}

/// Index (0–4) of the heart-rate zone `bpm` falls in, as a percentage of max HR.
///
/// Zone edges are 60 / 70 / 80 / 90 %. Returns zone 1 when max HR is unknown,
/// so an unconfigured profile reports no intensity rather than all of it.
pub fn hr_zone_index(bpm: u32, max_hr: u32) -> usize {
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

/// Format a pace in seconds per kilometre as `m:ss`.
pub fn format_pace_display(sec_per_km: u32) -> String {
    format!("{}:{:02}", sec_per_km / 60, sec_per_km % 60)
}

/// Format a distance in metres for display, in km once past a kilometre.
pub fn format_distance(distance_m: f32) -> String {
    if distance_m >= 1000.0 {
        format!("{:.2} km", distance_m / 1000.0)
    } else {
        format!("{:.0} m", distance_m)
    }
}

/// Average pace over a distance, as `m:ss/km`.
///
/// Returns an em dash when there is nothing to divide by — a zero-length or
/// instantaneous activity has no meaningful pace.
pub fn format_average_pace(distance_m: f32, duration_secs: u32) -> String {
    if distance_m < 1.0 || duration_secs == 0 {
        return "—".to_string();
    }
    let sec_per_km = (duration_secs as f32 / (distance_m / 1000.0)) as u32;
    format!("{}/km", format_pace_display(sec_per_km))
}

/// Seconds spent in each of the 5 heart-rate zones across local sessions.
pub fn compute_hr_zones(records: &[SessionRecord], max_hr: u32) -> [u32; 5] {
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

/// Seconds spent in each of the 7 power zones across local sessions.
///
/// Each ride is bucketed against the FTP it was ridden at, so raising FTP does
/// not retroactively demote past efforts into lower zones — `fallback_ftp`
/// applies only to rides recorded before FTP stamping existed.
pub fn compute_zone_seconds(records: &[SessionRecord], fallback_ftp: u32) -> [u32; 7] {
    let mut zone_secs = [0u32; 7];
    for record in records {
        for (zone, secs) in record
            .session
            .time_in_zones(fallback_ftp)
            .iter()
            .enumerate()
        {
            zone_secs[zone] += secs;
        }
    }
    zone_secs
}

/// Best average power for each [`CURVE_DURATIONS`] window.
///
/// Returns `(all_time, recent)` per duration, where `recent` covers rides on or
/// after `recent_cutoff`. A zero means no ride was long enough to fill that window.
pub fn compute_power_curve(records: &[SessionRecord], recent_cutoff: NaiveDate) -> Vec<(u32, u32)> {
    let mut all_time = vec![0u32; CURVE_DURATIONS.len()];
    let mut recent = vec![0u32; CURVE_DURATIONS.len()];
    for record in records {
        let is_recent = session_date(record) >= recent_cutoff;
        for (i, &dur) in CURVE_DURATIONS.iter().enumerate() {
            if let Some(peak) = record.session.peak_power_for_duration(dur) {
                if peak > all_time[i] {
                    all_time[i] = peak;
                }
                if is_recent && peak > recent[i] {
                    recent[i] = peak;
                }
            }
        }
    }
    all_time.into_iter().zip(recent).collect()
}

/// Best pace (seconds per kilometre) for each [`PACE_DISTANCES`] entry, from
/// cached run streams.
///
/// Returns `(all_time, recent)` per distance, where `recent` covers runs on or
/// after `recent_cutoff`. A zero means no run covered that distance. Streams
/// that fail to parse are skipped — the cache holds third-party JSON.
pub fn compute_pace_curve(
    run_streams: &[(NaiveDate, String)],
    recent_cutoff: NaiveDate,
) -> Vec<(u32, u32)> {
    let mut all_time = vec![0u32; PACE_DISTANCES.len()];
    let mut recent = vec![0u32; PACE_DISTANCES.len()];
    for (date, json) in run_streams {
        let is_recent = *date >= recent_cutoff;
        let Some(streams) = ActivityStreams::from_json(json) else {
            continue;
        };
        for (i, &dist) in PACE_DISTANCES.iter().enumerate() {
            let Some(elapsed) = streams.best_time_for_distance(dist) else {
                continue;
            };
            let pace = (elapsed as f32 * 1000.0 / dist).round() as u32;
            if pace == 0 {
                continue;
            }
            if all_time[i] == 0 || pace < all_time[i] {
                all_time[i] = pace;
            }
            if is_recent && (recent[i] == 0 || pace < recent[i]) {
                recent[i] = pace;
            }
        }
    }
    all_time.into_iter().zip(recent).collect()
}

/// Work and time totals for the current week and month.
#[derive(Debug, Clone, PartialEq)]
pub struct VolumeTotals {
    pub week_kj: f32,
    pub week_secs: u64,
    pub month_kj: f32,
    /// Local sessions plus synced activities, all time.
    pub activity_count: usize,
}

/// Sum work and time over the week and month containing `today`.
///
/// Local sessions and Intervals.icu activities are both counted; the caller is
/// responsible for passing only *unlinked* activities, so a ride that reached
/// Intervals.icu from this app is not counted twice.
pub fn compute_volume_totals(
    records: &[SessionRecord],
    icu_activities: &[IntervalsActivity],
    today: NaiveDate,
) -> VolumeTotals {
    let week_start = week_start_of(today);
    let month_start = month_start_of(today);

    let mut totals = VolumeTotals {
        week_kj: 0.0,
        week_secs: 0,
        month_kj: 0.0,
        activity_count: records.len() + icu_activities.len(),
    };

    for record in records {
        let date = session_date(record);
        let kj = record.session.kilojoules();
        if date >= week_start {
            totals.week_kj += kj;
            totals.week_secs += record.session.duration_secs();
        }
        if date >= month_start {
            totals.month_kj += kj;
        }
    }

    for act in icu_activities {
        // Intervals.icu reports average power and duration, not work — derive kJ
        // from the two. An activity missing either contributes no work.
        let kj = act
            .average_watts
            .zip(act.duration_secs)
            .map(|(w, d)| w as f32 * d as f32 / 1000.0)
            .unwrap_or(0.0);
        if act.date >= week_start {
            totals.week_kj += kj;
            totals.week_secs += act.duration_secs.unwrap_or(0) as u64;
        }
        if act.date >= month_start {
            totals.month_kj += kj;
        }
    }

    totals
}

/// Weekly training-stress totals for the `weeks` weeks ending with the week
/// containing `today`, oldest first.
///
/// Each entry is `(ISO week label, TSS)`. Local sessions and Intervals.icu TSS
/// pairs are summed; as with [`compute_volume_totals`], the caller must pass
/// only unlinked Intervals data.
pub fn compute_weekly_tss(
    records: &[SessionRecord],
    intervals_pairs: &[(NaiveDate, f32)],
    fallback_ftp: u32,
    today: NaiveDate,
    weeks: i64,
) -> Vec<(String, f32)> {
    let summaries: Vec<SessionSummary> = records.iter().map(|r| r.summary()).collect();
    compute_weekly_tss_from_summaries(&summaries, intervals_pairs, fallback_ftp, today, weeks)
}

/// [`compute_weekly_tss`] over summaries rather than whole rides.
///
/// The two exist because their callers hold different things. A page that has
/// already loaded full records passes those; the morning brief holds only
/// summaries, and loading every ride's data points to total a week of TSS would
/// read megabytes to produce six numbers. Only this one has an implementation —
/// the other maps and delegates — so they cannot drift apart.
pub fn compute_weekly_tss_from_summaries(
    summaries: &[SessionSummary],
    intervals_pairs: &[(NaiveDate, f32)],
    fallback_ftp: u32,
    today: NaiveDate,
    weeks: i64,
) -> Vec<(String, f32)> {
    let week_start = week_start_of(today);
    let mut out = Vec::with_capacity(weeks as usize);
    for i in (0..weeks).rev() {
        let ws = week_start - Duration::weeks(i);
        let we = ws + Duration::days(6);
        let tss_sessions: f32 = summaries
            .iter()
            .filter(|s| {
                let d = s.started_at.with_timezone(&Local).date_naive();
                d >= ws && d <= we
            })
            .filter_map(|s| s.tss(fallback_ftp))
            .sum();
        let tss_icu: f32 = intervals_pairs
            .iter()
            .filter(|(d, _)| *d >= ws && *d <= we)
            .map(|(_, t)| *t)
            .sum();
        out.push((format!("W{}", ws.iso_week().week()), tss_sessions + tss_icu));
    }
    out
}

/// A [`WELLNESS_WINDOW_DAYS`]-long series ending today, for one wellness metric.
///
/// Index 0 is the oldest day. Days with no entry — or a non-positive reading,
/// which the wellness sources use to mean "not measured" — stay at `0.0`, which
/// the sparkline draws as a gap.
pub fn build_wellness_series(
    wellness: &[WellnessEntry],
    today: NaiveDate,
    extractor: impl Fn(&WellnessEntry) -> Option<f32>,
) -> Vec<f32> {
    let len = WELLNESS_WINDOW_DAYS as usize;
    let mut vals = vec![0.0f32; len];
    for entry in wellness {
        let days_ago = (today - entry.date).num_days();
        if (0..WELLNESS_WINDOW_DAYS).contains(&days_ago) {
            let idx = (WELLNESS_WINDOW_DAYS - 1 - days_ago) as usize;
            if let Some(v) = extractor(entry) {
                if v > 0.0 {
                    vals[idx] = v;
                }
            }
        }
    }
    vals
}

/// Readings needed before a wellness baseline means anything.
///
/// An average of two mornings is not a norm, and a deviation measured against
/// one is noise wearing a percentage sign.
pub const MIN_WELLNESS_READINGS: usize = 4;

/// One wellness signal read against its own recent baseline.
///
/// The number and its context in one value, because handing them over
/// separately is what went wrong: a coach — human or otherwise — given a column
/// of HRV readings and asked to spot the outlier will sometimes just not, while
/// one told "34 % below your norm" cannot miss it.
#[derive(Debug, Clone, PartialEq)]
pub struct SignalReading {
    /// Rider-facing name, e.g. "HRV".
    pub label: &'static str,
    pub latest: f32,
    /// The mean of this signal over the window ending the day before.
    pub baseline: f32,
    /// Signed percentage away from `baseline`; positive is above it.
    pub deviation_pct: f32,
    /// Whether being above the baseline is the direction that means trouble.
    /// True for resting heart rate, false for HRV and sleep score.
    pub higher_is_worse: bool,
}

impl SignalReading {
    /// Whether this reading sits the wrong side of its baseline by `threshold`
    /// percent or more.
    pub fn is_adverse(&self, threshold: f32) -> bool {
        if self.higher_is_worse {
            self.deviation_pct >= threshold
        } else {
            self.deviation_pct <= -threshold
        }
    }
}

/// The mean of one wellness signal over the window ending the day before `day`.
///
/// Ends the day before on purpose: a baseline that includes the reading being
/// tested against it is diluted by that reading. Returns `None` until there are
/// `min_readings` of them, because an average of two mornings is not a norm.
pub fn wellness_baseline(
    wellness: &[WellnessEntry],
    day: NaiveDate,
    min_readings: usize,
    extract: impl Fn(&WellnessEntry) -> Option<f32>,
) -> Option<f32> {
    let prior = day.pred_opt()?;
    let vals: Vec<f32> = build_wellness_series(wellness, prior, extract)
        .into_iter()
        .filter(|&v| v > 0.0)
        .collect();
    (vals.len() >= min_readings).then(|| vals.iter().sum::<f32>() / vals.len() as f32)
}

/// Every wellness signal recorded for `day`, each against its own baseline.
///
/// Only signals that have both a reading for the day and a settled baseline
/// appear: a deviation from a norm that does not exist yet is not a fact.
pub fn wellness_readings(
    wellness: &[WellnessEntry],
    day: NaiveDate,
    min_readings: usize,
) -> Vec<SignalReading> {
    let Some(entry) = wellness.iter().find(|e| e.date == day) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    let mut add = |label, latest: Option<f32>, higher_is_worse, base: Option<f32>| {
        if let (Some(latest), Some(baseline)) = (latest.filter(|&v| v > 0.0), base) {
            if baseline > 0.0 {
                out.push(SignalReading {
                    label,
                    latest,
                    baseline,
                    deviation_pct: (latest - baseline) / baseline * 100.0,
                    higher_is_worse,
                });
            }
        }
    };

    add(
        "HRV",
        entry.hrv,
        false,
        wellness_baseline(wellness, day, min_readings, |e| e.hrv),
    );
    add(
        "Resting HR",
        entry.resting_hr.map(|v| v as f32),
        true,
        wellness_baseline(wellness, day, min_readings, |e| {
            e.resting_hr.map(|v| v as f32)
        }),
    );
    add(
        "Sleep score",
        entry.sleep_score.map(|v| v as f32),
        false,
        wellness_baseline(wellness, day, min_readings, |e| {
            e.sleep_score.map(|v| v as f32)
        }),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::session::{DataPoint, Session};
    use chrono::{TimeZone, Utc};

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("valid test date")
    }

    /// A session on `day` (local noon, so it lands on the same date in any
    /// plausible timezone) with one data point per second.
    fn record_on(day: NaiveDate, powers: &[u32], hrs: &[u32]) -> SessionRecord {
        let mut session = Session::new(None);
        session.started_at =
            Utc.from_utc_datetime(&day.and_hms_opt(12, 0, 0).expect("valid test time"));
        let n = powers.len().max(hrs.len());
        session.data_points = (0..n)
            .map(|i| DataPoint {
                elapsed_secs: i as u32,
                power_watts: powers.get(i).copied(),
                target_watts: None,
                heart_rate_bpm: hrs.get(i).copied(),
                cadence_rpm: None,
                speed_kmh: None,
                altitude_m: None,
                lat: None,
                lng: None,
            })
            .collect();
        session.ended_at = Some(session.started_at + Duration::seconds(n as i64));
        SessionRecord {
            session,
            workout_name: None,
            uploaded_to_icu: false,
        }
    }

    // ── hr_zone_index ────────────────────────────────────────────────────────

    #[test]
    fn should_return_zone_1_when_max_hr_is_unknown() {
        // An unconfigured profile must not report every beat as max effort.
        assert_eq!(hr_zone_index(180, 0), 0);
    }

    #[test]
    fn should_return_zone_1_at_exactly_60_percent_of_max_hr() {
        assert_eq!(hr_zone_index(120, 200), 0);
    }

    #[test]
    fn should_return_zone_2_just_above_60_percent_of_max_hr() {
        assert_eq!(hr_zone_index(122, 200), 1); // 61 %
    }

    #[test]
    fn should_return_zone_4_at_exactly_90_percent_of_max_hr() {
        assert_eq!(hr_zone_index(180, 200), 3);
    }

    #[test]
    fn should_return_zone_5_above_90_percent_of_max_hr() {
        assert_eq!(hr_zone_index(182, 200), 4); // 91 %
    }

    #[test]
    fn should_return_zone_5_when_bpm_exceeds_max_hr() {
        assert_eq!(hr_zone_index(210, 200), 4);
    }

    #[test]
    fn should_count_one_second_per_heart_rate_sample() {
        let records = vec![record_on(date(2026, 8, 3), &[], &[100, 100, 150, 190])];
        // 100/200 = 50 % → z1 ×2, 150/200 = 75 % → z3, 190/200 = 95 % → z5
        assert_eq!(compute_hr_zones(&records, 200), [2, 0, 1, 0, 1]);
    }

    #[test]
    fn should_report_no_hr_zone_time_when_sessions_have_no_hr() {
        let records = vec![record_on(date(2026, 8, 3), &[200, 200], &[])];
        assert_eq!(compute_hr_zones(&records, 200), [0; 5]);
    }

    // ── format_pace_display ──────────────────────────────────────────────────

    #[test]
    fn should_format_pace_with_zero_padded_seconds() {
        assert_eq!(format_pace_display(0), "0:00");
        assert_eq!(format_pace_display(59), "0:59");
        assert_eq!(format_pace_display(245), "4:05");
        assert_eq!(format_pace_display(600), "10:00");
    }

    // ── distance and pace formatting ─────────────────────────────────────────

    #[test]
    fn should_show_short_distances_in_metres_and_long_ones_in_kilometres() {
        assert_eq!(format_distance(0.0), "0 m");
        assert_eq!(format_distance(999.0), "999 m");
        assert_eq!(format_distance(1000.0), "1.00 km");
        assert_eq!(format_distance(5432.0), "5.43 km");
    }

    #[test]
    fn should_compute_average_pace_per_kilometre() {
        // 10 km in 50 minutes is 5:00/km.
        assert_eq!(format_average_pace(10_000.0, 3000), "5:00/km");
        // 5 km in 22:30 is 4:30/km.
        assert_eq!(format_average_pace(5_000.0, 1350), "4:30/km");
    }

    #[test]
    fn should_refuse_to_pace_an_activity_with_nothing_to_divide_by() {
        assert_eq!(format_average_pace(0.0, 3000), "—");
        assert_eq!(format_average_pace(10_000.0, 0), "—");
        assert_eq!(
            format_average_pace(0.5, 60),
            "—",
            "sub-metre is not a distance"
        );
    }

    // ── week/month boundaries ────────────────────────────────────────────────

    #[test]
    fn should_return_the_same_day_when_today_is_monday() {
        let monday = date(2026, 8, 3);
        assert_eq!(week_start_of(monday), monday);
    }

    #[test]
    fn should_walk_back_to_monday_from_sunday() {
        // Sunday must belong to the week that started six days earlier, not to
        // the week about to begin.
        assert_eq!(week_start_of(date(2026, 8, 9)), date(2026, 8, 3));
    }

    #[test]
    fn should_return_first_of_month() {
        assert_eq!(month_start_of(date(2026, 8, 31)), date(2026, 8, 1));
        assert_eq!(month_start_of(date(2026, 2, 1)), date(2026, 2, 1));
    }

    // ── compute_zone_seconds ─────────────────────────────────────────────────

    #[test]
    fn should_bucket_each_ride_against_the_ftp_it_was_ridden_at() {
        // 200 W is threshold at FTP 200, but only tempo at FTP 250. A ride
        // stamped with FTP 200 must stay in threshold after the rider improves.
        let mut old = record_on(date(2026, 6, 1), &[200; 10], &[]);
        old.session.ftp_watts = Some(200);
        let zones = compute_zone_seconds(&[old], 250);
        assert_eq!(zones.iter().sum::<u32>(), 10);
        assert_eq!(zones[3], 10, "expected 10 s at threshold, got {zones:?}");
    }

    #[test]
    fn should_fall_back_to_current_ftp_for_unstamped_rides() {
        let unstamped = record_on(date(2026, 6, 1), &[200; 10], &[]);
        assert!(unstamped.session.ftp_watts.is_none());
        let zones = compute_zone_seconds(&[unstamped], 200);
        assert_eq!(zones[3], 10, "expected 10 s at threshold, got {zones:?}");
    }

    // ── compute_power_curve ──────────────────────────────────────────────────

    #[test]
    fn should_take_the_best_peak_across_rides_per_duration() {
        let cutoff = date(2026, 8, 1);
        let records = vec![
            record_on(date(2026, 8, 5), &[300; 10], &[]),
            record_on(date(2026, 8, 6), &[250; 10], &[]),
        ];
        let curve = compute_power_curve(&records, cutoff);
        assert_eq!(curve[0].0, 300); // 5 s all-time
        assert_eq!(curve[1].0, 300); // 10 s all-time
    }

    #[test]
    fn should_exclude_older_rides_from_the_recent_series() {
        let cutoff = date(2026, 8, 1);
        let records = vec![
            record_on(date(2026, 5, 1), &[400; 10], &[]), // old, stronger
            record_on(date(2026, 8, 5), &[250; 10], &[]), // recent, weaker
        ];
        let curve = compute_power_curve(&records, cutoff);
        assert_eq!(curve[0].0, 400, "all-time keeps the old peak");
        assert_eq!(curve[0].1, 250, "recent must not inherit the old peak");
    }

    #[test]
    fn should_include_a_ride_on_the_cutoff_day_as_recent() {
        let cutoff = date(2026, 8, 1);
        let records = vec![record_on(cutoff, &[250; 10], &[])];
        assert_eq!(compute_power_curve(&records, cutoff)[0].1, 250);
    }

    #[test]
    fn should_report_zero_for_durations_no_ride_is_long_enough_to_fill() {
        let records = vec![record_on(date(2026, 8, 5), &[300; 10], &[])];
        let curve = compute_power_curve(&records, date(2026, 8, 1));
        assert_eq!(curve[0].0, 300); // 5 s — filled
        assert_eq!(curve[1].0, 300); // 10 s — exactly filled
        assert_eq!(curve[2].0, 0); // 30 s — not enough data
        assert_eq!(curve.len(), CURVE_DURATIONS.len());
    }

    #[test]
    fn should_return_an_all_zero_curve_for_no_rides() {
        let curve = compute_power_curve(&[], date(2026, 8, 1));
        assert_eq!(curve.len(), CURVE_DURATIONS.len());
        assert!(curve.iter().all(|&(a, r)| a == 0 && r == 0));
    }

    // ── compute_pace_curve ───────────────────────────────────────────────────

    #[test]
    fn should_skip_run_streams_that_fail_to_parse() {
        // The stream cache holds third-party JSON — malformed entries must not
        // take the whole curve down.
        let streams = vec![(date(2026, 8, 5), "not json at all".to_string())];
        let curve = compute_pace_curve(&streams, date(2026, 8, 1));
        assert_eq!(curve.len(), PACE_DISTANCES.len());
        assert!(curve.iter().all(|&(a, r)| a == 0 && r == 0));
    }

    #[test]
    fn should_have_a_label_for_every_pace_distance() {
        assert_eq!(PACE_DISTANCES.len(), PACE_LABELS.len());
    }

    // ── compute_volume_totals ────────────────────────────────────────────────

    fn icu(day: NaiveDate, watts: Option<u32>, secs: Option<u32>) -> IntervalsActivity {
        IntervalsActivity {
            icu_id: "i1".into(),
            date: day,
            name: "Ride".into(),
            tss: None,
            duration_secs: secs,
            average_watts: watts,
            normalized_watts: None,
            average_hr: None,
            max_hr: None,
            sport_type: "Ride".into(),
            start_datetime_local: None,
            distance_m: None,
            elevation_gain_m: None,
            average_cadence: None,
        }
    }

    #[test]
    fn should_exclude_last_weeks_ride_from_this_weeks_volume() {
        let today = date(2026, 8, 5); // Wednesday
        let records = vec![
            record_on(date(2026, 8, 3), &[200; 60], &[]), // Monday, this week
            record_on(date(2026, 8, 2), &[200; 60], &[]), // Sunday, last week
        ];
        let totals = compute_volume_totals(&records, &[], today);
        assert_eq!(totals.week_secs, 60, "only the Monday ride is in-week");
        assert_eq!(totals.activity_count, 2, "both count toward all-time");
    }

    #[test]
    fn should_count_last_weeks_ride_toward_the_month_when_it_is_the_same_month() {
        let today = date(2026, 8, 5);
        let records = vec![record_on(date(2026, 8, 2), &[200; 60], &[])];
        let totals = compute_volume_totals(&records, &[], today);
        assert_eq!(totals.week_kj, 0.0);
        assert!(totals.month_kj > 0.0);
    }

    #[test]
    fn should_derive_kilojoules_from_intervals_power_and_duration() {
        let today = date(2026, 8, 5);
        // 200 W for 3600 s = 720 kJ
        let acts = vec![icu(date(2026, 8, 4), Some(200), Some(3600))];
        let totals = compute_volume_totals(&[], &acts, today);
        assert!((totals.week_kj - 720.0).abs() < 0.01, "{}", totals.week_kj);
        assert_eq!(totals.week_secs, 3600);
    }

    #[test]
    fn should_contribute_no_work_when_intervals_activity_has_no_power() {
        let today = date(2026, 8, 5);
        let acts = vec![icu(date(2026, 8, 4), None, Some(3600))];
        let totals = compute_volume_totals(&[], &acts, today);
        assert_eq!(totals.week_kj, 0.0);
        assert_eq!(totals.week_secs, 3600, "time still counts without power");
        assert_eq!(totals.activity_count, 1);
    }

    #[test]
    fn should_report_zero_volume_with_no_activities() {
        let totals = compute_volume_totals(&[], &[], date(2026, 8, 5));
        assert_eq!(
            totals,
            VolumeTotals {
                week_kj: 0.0,
                week_secs: 0,
                month_kj: 0.0,
                activity_count: 0,
            }
        );
    }

    // ── compute_weekly_tss ───────────────────────────────────────────────────

    #[test]
    fn should_return_one_entry_per_week_oldest_first() {
        let weeks = compute_weekly_tss(&[], &[], 250, date(2026, 8, 5), 6);
        assert_eq!(weeks.len(), 6);
        assert_eq!(weeks[5].0, "W32", "last entry is the current week");
        assert!(weeks.iter().all(|(_, t)| *t == 0.0));
    }

    #[test]
    fn should_bucket_intervals_tss_into_the_week_it_falls_in() {
        let today = date(2026, 8, 5);
        let pairs = vec![
            (date(2026, 8, 3), 50.0),  // this week
            (date(2026, 7, 30), 80.0), // last week
        ];
        let weeks = compute_weekly_tss(&[], &pairs, 250, today, 6);
        assert_eq!(weeks[5].1, 50.0);
        assert_eq!(weeks[4].1, 80.0);
    }

    #[test]
    fn should_exclude_tss_from_outside_the_requested_window() {
        let today = date(2026, 8, 5);
        // 10 weeks back — outside a 6-week window.
        let pairs = vec![(date(2026, 5, 27), 100.0)];
        let weeks = compute_weekly_tss(&[], &pairs, 250, today, 6);
        assert!(weeks.iter().all(|(_, t)| *t == 0.0));
    }

    #[test]
    fn should_include_a_ride_on_the_last_day_of_a_week() {
        // Sunday is day 6 of its week — an inclusive end bound, easy to get wrong.
        let today = date(2026, 8, 5);
        let pairs = vec![(date(2026, 8, 9), 40.0)]; // Sunday of the current week
        let weeks = compute_weekly_tss(&[], &pairs, 250, today, 6);
        assert_eq!(weeks[5].1, 40.0);
    }

    // ── build_wellness_series ────────────────────────────────────────────────

    fn wellness_on(day: NaiveDate, hrv: Option<f32>) -> WellnessEntry {
        WellnessEntry {
            date: day,
            hrv,
            resting_hr: None,
            sleep_secs: None,
            sleep_score: None,
            steps: None,
            calories: None,
        }
    }

    #[test]
    fn should_place_todays_reading_last_in_the_series() {
        let today = date(2026, 8, 5);
        let series = build_wellness_series(&[wellness_on(today, Some(65.0))], today, |e| e.hrv);
        assert_eq!(series.len(), WELLNESS_WINDOW_DAYS as usize);
        assert_eq!(series[13], 65.0);
        assert!(series[..13].iter().all(|&v| v == 0.0));
    }

    #[test]
    fn should_place_the_oldest_in_window_reading_first() {
        let today = date(2026, 8, 5);
        let oldest = today - Duration::days(WELLNESS_WINDOW_DAYS - 1);
        let series = build_wellness_series(&[wellness_on(oldest, Some(50.0))], today, |e| e.hrv);
        assert_eq!(series[0], 50.0);
    }

    #[test]
    fn should_ignore_readings_older_than_the_window() {
        let today = date(2026, 8, 5);
        let too_old = today - Duration::days(WELLNESS_WINDOW_DAYS);
        let series = build_wellness_series(&[wellness_on(too_old, Some(50.0))], today, |e| e.hrv);
        assert!(series.iter().all(|&v| v == 0.0));
    }

    #[test]
    fn should_ignore_future_dated_readings() {
        // A device with a skewed clock must not write past the end of the series.
        let today = date(2026, 8, 5);
        let future = today + Duration::days(1);
        let series = build_wellness_series(&[wellness_on(future, Some(50.0))], today, |e| e.hrv);
        assert!(series.iter().all(|&v| v == 0.0));
    }

    #[test]
    fn should_treat_a_non_positive_reading_as_missing() {
        let today = date(2026, 8, 5);
        let series = build_wellness_series(&[wellness_on(today, Some(0.0))], today, |e| e.hrv);
        assert_eq!(series[13], 0.0);
    }

    // ── The curves and totals themselves ────────────────────────────────────
    //
    // Mutation testing found this half of the file open: every comparison in
    // `compute_pace_curve` could be flipped, and `+=` in `compute_volume_totals`
    // turned into `-=`, without a test objecting. These pin the numbers rather
    // than their direction, and put rides on the boundaries of the windows they
    // are counted into.

    /// An Intervals.icu activity carrying only what the volume totals read.
    fn icu_on(day: NaiveDate, watts: u32, secs: u32) -> IntervalsActivity {
        IntervalsActivity {
            icu_id: format!("icu-{day}"),
            date: day,
            name: "Ride".into(),
            tss: None,
            duration_secs: Some(secs),
            average_watts: Some(watts),
            normalized_watts: None,
            average_hr: None,
            max_hr: None,
            sport_type: "Ride".into(),
            start_datetime_local: None,
            distance_m: None,
            elevation_gain_m: None,
            average_cadence: None,
        }
    }

    /// A run at a constant `speed_ms` for `secs`, as the cached stream JSON.
    fn run_stream(speed_ms: f32, secs: u32) -> String {
        let times: Vec<String> = (0..=secs).map(|t| t.to_string()).collect();
        let dists: Vec<String> = (0..=secs)
            .map(|t| (t as f32 * speed_ms).to_string())
            .collect();
        format!(
            r#"[{{"type":"time","data":[{}]}},{{"type":"distance","data":[{}]}}]"#,
            times.join(","),
            dists.join(",")
        )
    }

    fn summary_on(day: NaiveDate, np: f32, ftp: u32, secs: u64) -> SessionSummary {
        SessionSummary {
            id: 1,
            started_at: Utc.from_utc_datetime(&day.and_hms_opt(12, 0, 0).expect("valid time")),
            duration_secs: secs,
            normalised_power: Some(np),
            average_power: Some(np),
            kilojoules: 0.0,
            ftp_watts: Some(ftp),
            rpe: None,
            workout_name: None,
            uploaded_to_icu: false,
            icu_id: None,
        }
    }

    #[test]
    fn should_pace_an_activity_that_covers_exactly_one_metre() {
        // The guard is "under a metre", so a metre itself has a pace.
        assert_ne!(format_average_pace(1.0, 10), "—");
        assert_eq!(format_average_pace(0.99, 10), "—");
    }

    #[test]
    fn should_keep_the_recent_power_best_separate_from_the_all_time_best() {
        let cutoff = date(2026, 8, 1);
        // Recent first, so a rule that let an old ride through would overwrite
        // the recent best rather than merely failing to be excluded by it.
        let records = [
            record_on(date(2026, 8, 10), &[200; 60], &[]),
            record_on(date(2026, 6, 1), &[300; 60], &[]),
        ];
        let curve = compute_power_curve(&records, cutoff);
        assert_eq!(curve[0], (300, 200), "all-time 300 W, recent 200 W");
    }

    #[test]
    fn should_report_a_pace_in_seconds_per_kilometre() {
        // 5 m/s covers the 400 m in 80 s, which is 200 s/km.
        let runs = [(date(2026, 8, 10), run_stream(5.0, 200))];
        let curve = compute_pace_curve(&runs, date(2026, 8, 1));
        assert_eq!(curve[0].0, 200, "400 m at 5 m/s is a 200 s/km pace");
    }

    #[test]
    fn should_keep_the_fastest_pace_whichever_order_the_runs_arrive_in() {
        let cutoff = date(2026, 8, 1);
        let slow = (date(2026, 8, 10), run_stream(5.0, 200)); // 200 s/km
        let fast = (date(2026, 8, 11), run_stream(8.0, 200)); // 125 s/km
                                                              // Lower is better here, which is the whole reason this comparison is
                                                              // easy to get backwards.
        let a = compute_pace_curve(&[slow.clone(), fast.clone()], cutoff);
        let b = compute_pace_curve(&[fast, slow], cutoff);
        assert_eq!(a[0].0, 125);
        assert_eq!(
            b[0].0, 125,
            "the order runs are read in cannot change the best"
        );
    }

    #[test]
    fn should_keep_the_recent_pace_best_separate_from_the_all_time_best() {
        let cutoff = date(2026, 8, 1);
        let runs = [
            (date(2026, 6, 1), run_stream(8.0, 200)), // 125 s/km, too old
            (date(2026, 8, 10), run_stream(5.0, 200)), // 200 s/km, recent
        ];
        let curve = compute_pace_curve(&runs, cutoff);
        assert_eq!(curve[0], (125, 200));
    }

    #[test]
    fn should_leave_a_distance_no_run_covered_at_zero() {
        let runs = [(date(2026, 8, 10), run_stream(5.0, 200))]; // 1000 m
        let curve = compute_pace_curve(&runs, date(2026, 8, 1));
        // The marathon entry: nothing has covered it.
        assert_eq!(curve[PACE_DISTANCES.len() - 1], (0, 0));
    }

    #[test]
    fn should_add_up_the_work_of_the_week_from_both_sources() {
        let today = date(2026, 8, 5);
        // 200 W for 60 s is 12 kJ; 100 W for 600 s is 60 kJ.
        let records = [record_on(today, &[200; 60], &[])];
        let icu = [icu_on(today, 100, 600)];
        let totals = compute_volume_totals(&records, &icu, today);
        assert!(
            (totals.week_kj - 72.0).abs() < 0.01,
            "expected 72 kJ, got {}",
            totals.week_kj
        );
        assert_eq!(totals.week_secs, 660);
        assert_eq!(totals.activity_count, 2);
    }

    #[test]
    fn should_leave_last_weeks_work_out_of_this_weeks_total() {
        let today = date(2026, 8, 5);
        let last_week = week_start_of(today) - Duration::days(1);
        let records = [record_on(last_week, &[200; 60], &[])];
        let icu = [icu_on(last_week, 100, 600)];
        let totals = compute_volume_totals(&records, &icu, today);
        assert_eq!(totals.week_kj, 0.0);
        assert_eq!(totals.week_secs, 0);
        // Both still happened, and both are still this month.
        assert_eq!(totals.activity_count, 2);
    }

    #[test]
    fn should_count_a_ride_on_either_edge_of_the_week() {
        let today = date(2026, 8, 5);
        let monday = week_start_of(today);
        let sunday = monday + Duration::days(6);
        let summaries = [
            summary_on(monday, 200.0, 200, 3600),
            summary_on(sunday, 200.0, 200, 3600),
            summary_on(monday - Duration::days(1), 200.0, 200, 3600),
            summary_on(sunday + Duration::days(1), 200.0, 200, 3600),
        ];
        let weekly = compute_weekly_tss_from_summaries(&summaries, &[], 200, today, 1);
        // An hour at FTP is 100 TSS, and exactly two of the four rides are in
        // the week — the ones on its first and last day.
        assert!(
            (weekly[0].1 - 200.0).abs() < 0.01,
            "expected 200 TSS, got {}",
            weekly[0].1
        );
    }

    #[test]
    fn should_call_a_reading_adverse_only_on_its_own_bad_side() {
        let hrv = SignalReading {
            label: "HRV",
            latest: 50.0,
            baseline: 50.0,
            deviation_pct: 0.0,
            higher_is_worse: false,
        };
        // On the baseline is not adverse in either direction.
        assert!(!hrv.is_adverse(5.0));
        // HRV below its norm is the bad side; above it is not.
        assert!(SignalReading {
            deviation_pct: -6.0,
            ..hrv.clone()
        }
        .is_adverse(5.0));
        assert!(!SignalReading {
            deviation_pct: 6.0,
            ..hrv.clone()
        }
        .is_adverse(5.0));
        // Resting heart rate reads the other way round.
        let rhr = SignalReading {
            label: "Resting HR",
            higher_is_worse: true,
            ..hrv
        };
        assert!(SignalReading {
            deviation_pct: 6.0,
            ..rhr.clone()
        }
        .is_adverse(5.0));
        assert!(!SignalReading {
            deviation_pct: -6.0,
            ..rhr
        }
        .is_adverse(5.0));
    }

    #[test]
    fn should_read_each_signal_against_its_own_baseline() {
        let today = date(2026, 8, 5);
        // Six mornings at 50 bpm, then 55 today: 10 % above the norm.
        let mut wellness: Vec<WellnessEntry> = (1..=6)
            .map(|ago| WellnessEntry {
                date: today - Duration::days(ago),
                hrv: Some(100.0),
                resting_hr: Some(50),
                sleep_secs: None,
                sleep_score: None,
                steps: None,
                calories: None,
            })
            .collect();
        wellness.push(WellnessEntry {
            date: today,
            resting_hr: Some(55),
            hrv: Some(90.0),
            ..wellness[0].clone()
        });

        let readings = wellness_readings(&wellness, today, MIN_WELLNESS_READINGS);
        let rhr = readings
            .iter()
            .find(|r| r.label == "Resting HR")
            .expect("a resting heart rate reading");
        assert_eq!(rhr.latest, 55.0);
        assert_eq!(rhr.baseline, 50.0);
        assert!(
            (rhr.deviation_pct - 10.0).abs() < 0.01,
            "expected 10 % above the norm, got {}",
            rhr.deviation_pct
        );
        assert!(rhr.higher_is_worse);

        let hrv = readings
            .iter()
            .find(|r| r.label == "HRV")
            .expect("an HRV reading");
        assert!(
            (hrv.deviation_pct + 10.0).abs() < 0.01,
            "expected 10 % below the norm, got {}",
            hrv.deviation_pct
        );
        assert!(!hrv.higher_is_worse);
    }

    #[test]
    fn should_read_no_signals_for_a_day_with_no_entry_of_its_own() {
        // The staleness trap again: yesterday's numbers are not this morning's.
        let today = date(2026, 8, 5);
        let wellness: Vec<WellnessEntry> = (1..=6)
            .map(|ago| WellnessEntry {
                date: today - Duration::days(ago),
                hrv: Some(100.0),
                resting_hr: Some(50),
                sleep_secs: None,
                sleep_score: None,
                steps: None,
                calories: None,
            })
            .collect();
        assert!(wellness_readings(&wellness, today, MIN_WELLNESS_READINGS).is_empty());
    }

    #[test]
    fn should_add_up_the_months_work_from_both_sources() {
        // Placed on the first of the month, the edge the ride is counted from.
        let today = date(2026, 8, 5);
        let first = month_start_of(today);
        let records = [record_on(first, &[200; 60], &[])];
        let icu = [icu_on(first, 100, 600)];
        let totals = compute_volume_totals(&records, &icu, today);
        assert!(
            (totals.month_kj - 72.0).abs() < 0.01,
            "expected 72 kJ, got {}",
            totals.month_kj
        );
    }

    #[test]
    fn should_leave_last_months_work_out_of_this_months_total() {
        let today = date(2026, 8, 5);
        let last_month = month_start_of(today) - Duration::days(1);
        let records = [record_on(last_month, &[200; 60], &[])];
        let icu = [icu_on(last_month, 100, 600)];
        let totals = compute_volume_totals(&records, &icu, today);
        assert_eq!(totals.month_kj, 0.0);
    }
}
