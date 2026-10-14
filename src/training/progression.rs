//! "How does this ride compare with the last time I did it?"
//!
//! The rider repeats named efforts — a route, a structured workout, a regular
//! loop — and the interesting question afterwards is not what the numbers were
//! but whether they moved. This module finds the earlier attempts at the same
//! effort and works out what changed.
//!
//! Two sources feed it, and neither is sufficient alone:
//!
//! * the app's own `sessions`, which carry the rider's own naming and RPE but
//!   record no average heart rate outside the samples;
//! * `intervals_activities`, which carry heart rate and a full history reaching
//!   back before the app existed, but are named by whatever wrote them.
//!
//! A ride that has round-tripped through Garmin appears in both, so the two are
//! merged on `icu_id` rather than concatenated. The app's title wins: Garmin
//! renames every indoor ride to "Indoor Cycling", which would file a cruise
//! interval set, a VO₂max session and a virtual road ride under one name and
//! then compare them to each other.

use chrono::NaiveDate;

use crate::data::db::{IntervalsActivity, SessionSummary};
use crate::data::sport::{is_cycling, is_indoor};

/// Two efforts count as the same route only when their distances agree this
/// closely. Riding "the same loop" 20 % shorter is a different effort, and
/// calling one of them faster than the other would be nonsense.
const SAME_DISTANCE_TOLERANCE: f32 = 0.05;

/// One attempt at a named effort, from whichever source described it best.
#[derive(Debug, Clone, PartialEq)]
pub struct Effort {
    pub date: NaiveDate,
    /// As shown to the rider — the app's title where there is one.
    pub name: String,
    pub duration_secs: u32,
    pub normalised_power: Option<u32>,
    pub average_power: Option<u32>,
    pub average_hr: Option<u32>,
    pub distance_m: Option<f32>,
    pub rpe: Option<u8>,
    /// Ridden on a trainer rather than on the road. A GPX route ridden indoors
    /// meets its outdoor rides by name, and power compares fairly across the
    /// two — but time and heart rate do not, so those are only ever compared
    /// within one setting.
    pub indoor: bool,
}

impl Effort {
    /// The figure efforts are ranked and compared on.
    ///
    /// Normalised power describes a variable ride better than the mean, but a
    /// ride imported without it still has an average worth comparing, so the
    /// average stands in rather than dropping the effort from the history.
    pub fn power(&self) -> Option<u32> {
        self.normalised_power.or(self.average_power)
    }

    /// Watts per heartbeat — the same work at a lower heart rate is the
    /// clearest evidence of aerobic fitness the app can compute from stored
    /// figures alone.
    pub fn efficiency(&self) -> Option<f32> {
        let power = self.power()?;
        let hr = self.average_hr.filter(|&h| h > 0)?;
        Some(power as f32 / hr as f32)
    }

    /// True when `other` covered close enough to the same ground that comparing
    /// elapsed times is meaningful.
    pub fn same_distance_as(&self, other: &Effort) -> bool {
        match (self.distance_m, other.distance_m) {
            (Some(a), Some(b)) if a > 0.0 && b > 0.0 => {
                (a - b).abs() / a.max(b) <= SAME_DISTANCE_TOLERANCE
            }
            _ => false,
        }
    }
}

/// Fold a display name down to a key two attempts at the same effort share.
///
/// Case is folded explicitly rather than through [`str::to_lowercase`] because
/// Turkish breaks the usual assumption: `İ` lowercases to `i` plus a combining
/// dot, so "Kadıköy" and "Kadiköy" — the same place, spelled by two different
/// devices — would not meet. Dotted and dotless i are mapped together here, on
/// purpose: this is a matching key, never anything the rider reads.
///
/// The app prefixes a route ride with "Virtual", and the same route ridden
/// outdoors is the same effort, so a leading "virtual" is dropped.
pub fn normalise_name(raw: &str) -> String {
    let folded: String = raw
        .chars()
        .map(|c| match c {
            'ı' | 'İ' | 'I' | 'i' => 'i',
            other => other,
        })
        .flat_map(|c| c.to_lowercase())
        .collect();

    let collapsed = folded.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
        .strip_prefix("virtual ")
        .unwrap_or(&collapsed)
        .to_string()
}

/// Build the effort history from both stores, newest last.
///
/// Sessions already represented in `intervals_activities` are merged with their
/// counterpart rather than dropped: the session supplies the name and RPE, the
/// activity supplies heart rate and distance. Rides that are not cycling are
/// left out — a run named like a ride is not something to compare watts across.
pub fn build_history(sessions: &[SessionSummary], activities: &[IntervalsActivity]) -> Vec<Effort> {
    let mut efforts: Vec<Effort> = Vec::new();

    // Sessions first, each absorbing its Intervals.icu twin where it has one.
    for s in sessions {
        let twin = s
            .icu_id
            .as_ref()
            .and_then(|id| activities.iter().find(|a| &a.icu_id == id));

        efforts.push(Effort {
            date: s.started_at.date_naive(),
            name: s.workout_name.clone().unwrap_or_else(|| "Ride".to_string()),
            duration_secs: s.duration_secs as u32,
            normalised_power: s
                .normalised_power
                .map(|v| v as u32)
                .or_else(|| twin.and_then(|a| a.normalized_watts)),
            average_power: s
                .average_power
                .map(|v| v as u32)
                .or_else(|| twin.and_then(|a| a.average_watts)),
            // Not stored on the session row at all, so the twin is the only
            // source of a heart rate for the app's own rides.
            average_hr: twin.and_then(|a| a.average_hr),
            distance_m: twin.and_then(|a| a.distance_m),
            rpe: s.rpe,
            // The app only rides indoors, whatever the synced twin calls it.
            indoor: true,
        });
    }

    // Then every activity that no session claimed.
    let claimed: Vec<&String> = sessions.iter().filter_map(|s| s.icu_id.as_ref()).collect();
    for a in activities {
        if claimed.contains(&&a.icu_id) {
            continue;
        }
        if !is_cycling(&a.sport_type) {
            continue;
        }
        efforts.push(Effort {
            date: a.date,
            name: a.name.clone(),
            duration_secs: a.duration_secs.unwrap_or(0),
            normalised_power: a.normalized_watts,
            average_power: a.average_watts,
            average_hr: a.average_hr,
            distance_m: a.distance_m,
            rpe: None,
            indoor: is_indoor(&a.sport_type),
        });
    }

    efforts.sort_by_key(|e| e.date);
    efforts
}

/// Every earlier attempt at the effort `name` refers to, oldest first.
///
/// `on_or_after` is excluded along with anything later, so a ride never counts
/// itself as its own predecessor — the ride being described is often already in
/// the history by the time this is asked.
pub fn prior_efforts(history: &[Effort], name: &str, before: NaiveDate) -> Vec<Effort> {
    let key = normalise_name(name);
    history
        .iter()
        .filter(|e| e.date < before && normalise_name(&e.name) == key)
        .cloned()
        .collect()
}

/// Where an effort stands against the ones before it.
#[derive(Debug, Clone, PartialEq)]
pub struct Comparison {
    /// How many attempts there have been in total, this one included.
    pub attempt: usize,
    /// The date of the earliest attempt, for "N efforts since …".
    pub since: NaiveDate,
    /// The attempt immediately before this one, indoors or out.
    pub previous: Effort,
    /// The most recent earlier attempt ridden in the same setting as this one
    /// — indoors against indoors, outdoors against outdoors. What heart rate
    /// and time are compared against; see [`Effort::indoor`].
    pub previous_like: Option<Effort>,
    /// How many earlier attempts were ridden in the other setting.
    pub other_setting: usize,
    /// The strongest previous attempt by power.
    pub best: Option<Effort>,
    /// 1 when this ride is the strongest yet; `None` when it has no power.
    pub power_rank: Option<usize>,
    /// Power series across every attempt, oldest first, for the sparkline.
    /// Attempts with no power reading are `0.0`, which the sparkline skips.
    pub power_series: Vec<f32>,
}

impl Comparison {
    /// True when nothing beats this ride on power.
    pub fn is_best(&self) -> bool {
        self.power_rank == Some(1)
    }

    /// The earlier attempt to compare aerobic efficiency against, when both
    /// rides have one.
    ///
    /// Same setting only: indoor heat lifts heart rate at a given power, so an
    /// indoor ride set against an outdoor one would read as lost fitness.
    pub fn efficiency_baseline(&self, current: &Effort) -> Option<&Effort> {
        current.efficiency()?;
        self.previous_like
            .as_ref()
            .filter(|e| e.efficiency().is_some())
    }

    /// The earlier attempt to compare elapsed time against, if any.
    ///
    /// Same setting and the same ground only. Indoor speed comes from the
    /// app's physics model, with no wind, traffic or braking; outdoor speed
    /// comes from the road, so a gap between the two says nothing about the
    /// rider.
    pub fn time_baseline(&self, current: &Effort) -> Option<&Effort> {
        if current.duration_secs == 0 {
            return None;
        }
        self.previous_like
            .as_ref()
            .filter(|e| e.duration_secs > 0 && current.same_distance_as(e))
    }
}

/// Place `current` against its earlier attempts.
///
/// Returns `None` when there is nothing to compare against — one ride is not a
/// progression, and a card claiming otherwise would be noise.
pub fn compare(current: &Effort, priors: &[Effort]) -> Option<Comparison> {
    let previous = priors.iter().max_by_key(|e| e.date)?.clone();
    let since = priors.iter().map(|e| e.date).min()?;
    let previous_like = priors
        .iter()
        .filter(|e| e.indoor == current.indoor)
        .max_by_key(|e| e.date)
        .cloned();
    let other_setting = priors.iter().filter(|e| e.indoor != current.indoor).count();

    let best = priors
        .iter()
        .filter(|e| e.power().is_some())
        .max_by_key(|e| e.power().unwrap_or(0))
        .cloned();

    // Rank counts how many earlier attempts were stronger, so an equal effort
    // ties rather than displacing the ride that got there first.
    let power_rank = current.power().map(|p| {
        1 + priors
            .iter()
            .filter(|e| e.power().is_some_and(|prior| prior > p))
            .count()
    });

    let mut ordered: Vec<&Effort> = priors.iter().collect();
    ordered.sort_by_key(|e| e.date);
    let mut power_series: Vec<f32> = ordered
        .iter()
        .map(|e| e.power().unwrap_or(0) as f32)
        .collect();
    power_series.push(current.power().unwrap_or(0) as f32);

    Some(Comparison {
        attempt: priors.len() + 1,
        since,
        previous,
        previous_like,
        other_setting,
        best,
        power_rank,
        power_series,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, TimeZone, Utc};

    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 6, d).expect("hardcoded valid date")
    }

    fn effort(d: u32, name: &str, power: Option<u32>) -> Effort {
        Effort {
            date: day(d),
            name: name.to_string(),
            duration_secs: 3600,
            normalised_power: power,
            average_power: power,
            average_hr: None,
            distance_m: None,
            rpe: None,
            indoor: true,
        }
    }

    // ── Name normalisation ────────────────────────────────────────────────────

    #[test]
    fn should_match_dotted_and_dotless_i_as_the_same_place() {
        // The app records "Kadıköy"; Garmin sends back "Kadiköy".
        assert_eq!(normalise_name("Kadıköy"), normalise_name("Kadiköy"));
    }

    #[test]
    fn should_match_a_capital_dotted_i_without_leaving_a_combining_mark() {
        // to_lowercase() alone turns 'İ' into "i\u{307}", which matches nothing.
        let folded = normalise_name("İstanbul");
        assert_eq!(folded, "istanbul");
        assert!(!folded.chars().any(|c| c == '\u{307}'));
    }

    #[test]
    fn should_treat_a_virtual_ride_as_the_same_effort_as_the_road_one() {
        assert_eq!(
            normalise_name("Virtual Maltepe Road Cycling"),
            normalise_name("Maltepe Road Cycling")
        );
    }

    #[test]
    fn should_only_strip_virtual_from_the_front() {
        // A route that merely contains the word keeps it.
        assert_eq!(normalise_name("Almost Virtual Loop"), "almost virtual loop");
    }

    #[test]
    fn should_collapse_case_and_stray_whitespace() {
        assert_eq!(normalise_name("  Cruise   INTERVALS "), "cruise intervals");
    }

    #[test]
    fn should_not_conflate_two_different_efforts() {
        assert_ne!(
            normalise_name("Konak - Base"),
            normalise_name("Konak - Threshold")
        );
    }

    // ── Effort figures ────────────────────────────────────────────────────────

    #[test]
    fn should_prefer_normalised_power_over_the_average() {
        let mut e = effort(1, "Ride", Some(200));
        e.average_power = Some(180);
        assert_eq!(e.power(), Some(200));
    }

    #[test]
    fn should_fall_back_to_average_power_when_normalised_is_missing() {
        let mut e = effort(1, "Ride", None);
        e.average_power = Some(180);
        assert_eq!(e.power(), Some(180));
    }

    #[test]
    fn should_compute_watts_per_beat() {
        let mut e = effort(1, "Ride", Some(200));
        e.average_hr = Some(160);
        assert_eq!(e.efficiency(), Some(1.25));
    }

    #[test]
    fn should_not_divide_by_a_zero_heart_rate() {
        let mut e = effort(1, "Ride", Some(200));
        e.average_hr = Some(0);
        assert_eq!(e.efficiency(), None);
    }

    #[test]
    fn should_call_two_rides_the_same_distance_within_tolerance() {
        let mut a = effort(1, "Loop", Some(200));
        let mut b = effort(2, "Loop", Some(200));
        a.distance_m = Some(40_000.0);
        b.distance_m = Some(41_000.0); // 2.5 % apart
        assert!(a.same_distance_as(&b));
    }

    #[test]
    fn should_refuse_to_compare_times_across_different_distances() {
        // The rider's real Maltepe rides run from 5 km to 63 km.
        let mut a = effort(1, "Maltepe", Some(200));
        let mut b = effort(2, "Maltepe", Some(200));
        a.distance_m = Some(40_000.0);
        b.distance_m = Some(63_000.0);
        assert!(!a.same_distance_as(&b));
    }

    #[test]
    fn should_not_claim_the_same_distance_when_one_is_unknown() {
        let mut a = effort(1, "Loop", Some(200));
        a.distance_m = Some(40_000.0);
        let b = effort(2, "Loop", Some(200));
        assert!(!a.same_distance_as(&b));
    }

    // ── History assembly ──────────────────────────────────────────────────────

    fn summary(
        id: i64,
        at: DateTime<Utc>,
        name: &str,
        np: Option<f32>,
        icu_id: Option<&str>,
    ) -> SessionSummary {
        SessionSummary {
            id,
            started_at: at,
            duration_secs: 3600,
            normalised_power: np,
            average_power: np,
            kilojoules: 0.0,
            ftp_watts: Some(250),
            rpe: Some(7),
            workout_name: Some(name.to_string()),
            uploaded_to_icu: icu_id.is_some(),
            icu_id: icu_id.map(str::to_string),
            integrity: Default::default(),
            integrity_dismissed: false,
        }
    }

    fn activity(
        icu_id: &str,
        d: u32,
        name: &str,
        sport: &str,
        np: Option<u32>,
    ) -> IntervalsActivity {
        IntervalsActivity {
            icu_id: icu_id.to_string(),
            date: day(d),
            name: name.to_string(),
            tss: None,
            duration_secs: Some(3600),
            average_watts: np,
            normalized_watts: np,
            average_hr: Some(150),
            max_hr: None,
            sport_type: sport.to_string(),
            start_datetime_local: None,
            distance_m: Some(40_000.0),
            elevation_gain_m: None,
            average_cadence: None,
        }
    }

    fn at(d: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, d, 10, 0, 0)
            .single()
            .expect("hardcoded valid timestamp")
    }

    #[test]
    fn should_count_a_round_tripped_ride_once() {
        // The same ride: recorded here, uploaded, and synced back from Garmin.
        let sessions = vec![summary(
            1,
            at(4),
            "Cruise Intervals",
            Some(210.0),
            Some("i1"),
        )];
        let activities = vec![activity(
            "i1",
            4,
            "Indoor Cycling",
            "VirtualRide",
            Some(210),
        )];

        let history = build_history(&sessions, &activities);
        assert_eq!(history.len(), 1, "one ride must not become two");
    }

    #[test]
    fn should_keep_the_riders_own_name_over_the_one_garmin_invented() {
        // Garmin files every indoor ride as "Indoor Cycling". Trusting that
        // would compare a cruise interval set against a VO₂max session.
        let sessions = vec![
            summary(1, at(4), "Cruise Intervals", Some(210.0), Some("i1")),
            summary(2, at(5), "VO₂Max Staircase", Some(240.0), Some("i2")),
        ];
        let activities = vec![
            activity("i1", 4, "Indoor Cycling", "VirtualRide", Some(210)),
            activity("i2", 5, "Indoor Cycling", "VirtualRide", Some(240)),
        ];

        let history = build_history(&sessions, &activities);
        let names: Vec<&str> = history.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"Cruise Intervals"));
        assert!(names.contains(&"VO₂Max Staircase"));
        assert!(!names.contains(&"Indoor Cycling"));
    }

    #[test]
    fn should_take_heart_rate_from_the_synced_twin() {
        // Sessions store no average heart rate, so without the merge the
        // efficiency line could never appear for the app's own rides.
        let sessions = vec![summary(
            1,
            at(4),
            "Cruise Intervals",
            Some(210.0),
            Some("i1"),
        )];
        let activities = vec![activity(
            "i1",
            4,
            "Indoor Cycling",
            "VirtualRide",
            Some(210),
        )];

        let history = build_history(&sessions, &activities);
        assert_eq!(history[0].average_hr, Some(150));
        assert_eq!(history[0].rpe, Some(7), "RPE still comes from the session");
    }

    #[test]
    fn should_leave_out_activities_that_are_not_cycling() {
        let activities = vec![
            activity("i1", 4, "Konak - Base", "Run", None),
            activity("i2", 5, "Maltepe Road Cycling", "Ride", Some(190)),
        ];
        let history = build_history(&[], &activities);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].name, "Maltepe Road Cycling");
    }

    #[test]
    fn should_return_the_history_oldest_first() {
        let activities = vec![
            activity("i2", 9, "Loop", "Ride", Some(200)),
            activity("i1", 2, "Loop", "Ride", Some(180)),
        ];
        let history = build_history(&[], &activities);
        assert_eq!(history[0].date, day(2));
        assert_eq!(history[1].date, day(9));
    }

    // ── Prior efforts ─────────────────────────────────────────────────────────

    #[test]
    fn should_find_earlier_attempts_at_the_same_effort() {
        let history = vec![
            effort(1, "Maltepe Road Cycling", Some(180)),
            effort(3, "Kadiköy Road Cycling", Some(190)),
            effort(5, "Virtual Maltepe Road Cycling", Some(200)),
        ];
        let priors = prior_efforts(&history, "Maltepe Road Cycling", day(9));
        assert_eq!(priors.len(), 2, "the virtual ride is the same route");
    }

    #[test]
    fn should_not_let_a_ride_be_its_own_predecessor() {
        let history = vec![effort(5, "Loop", Some(200))];
        assert!(prior_efforts(&history, "Loop", day(5)).is_empty());
    }

    #[test]
    fn should_ignore_attempts_made_after_the_ride_being_described() {
        // Looking back at an old ride compares it with what came before it.
        let history = vec![effort(1, "Loop", Some(180)), effort(9, "Loop", Some(230))];
        let priors = prior_efforts(&history, "Loop", day(5));
        assert_eq!(priors.len(), 1);
        assert_eq!(priors[0].date, day(1));
    }

    // ── Comparison ────────────────────────────────────────────────────────────

    #[test]
    fn should_say_nothing_about_a_first_attempt() {
        assert!(compare(&effort(5, "Loop", Some(200)), &[]).is_none());
    }

    #[test]
    fn should_compare_against_the_most_recent_attempt() {
        let priors = vec![
            effort(1, "Loop", Some(180)),
            effort(4, "Loop", Some(210)),
            effort(2, "Loop", Some(190)),
        ];
        let c = compare(&effort(9, "Loop", Some(200)), &priors).expect("three priors");
        assert_eq!(c.previous.date, day(4), "most recent, not the strongest");
        assert_eq!(c.attempt, 4);
        assert_eq!(c.since, day(1));
    }

    #[test]
    fn should_rank_a_new_best_first() {
        let priors = vec![effort(1, "Loop", Some(180)), effort(4, "Loop", Some(210))];
        let c = compare(&effort(9, "Loop", Some(230)), &priors).expect("two priors");
        assert_eq!(c.power_rank, Some(1));
        assert!(c.is_best());
        assert_eq!(c.best.expect("a strongest prior").power(), Some(210));
    }

    #[test]
    fn should_rank_a_middling_ride_by_how_many_beat_it() {
        let priors = vec![
            effort(1, "Loop", Some(180)),
            effort(2, "Loop", Some(240)),
            effort(4, "Loop", Some(210)),
        ];
        let c = compare(&effort(9, "Loop", Some(200)), &priors).expect("three priors");
        assert_eq!(c.power_rank, Some(3), "two earlier rides were stronger");
        assert!(!c.is_best());
    }

    #[test]
    fn should_let_an_equalled_effort_tie_rather_than_take_the_record() {
        let priors = vec![effort(1, "Loop", Some(200))];
        let c = compare(&effort(9, "Loop", Some(200)), &priors).expect("one prior");
        assert_eq!(c.power_rank, Some(1));
    }

    #[test]
    fn should_rank_nothing_when_the_ride_recorded_no_power() {
        let priors = vec![effort(1, "Loop", Some(200))];
        let c = compare(&effort(9, "Loop", None), &priors).expect("one prior");
        assert_eq!(c.power_rank, None);
        assert!(!c.is_best());
    }

    #[test]
    fn should_build_a_power_series_ending_with_this_ride() {
        let priors = vec![effort(4, "Loop", Some(210)), effort(1, "Loop", Some(180))];
        let c = compare(&effort(9, "Loop", Some(230)), &priors).expect("two priors");
        assert_eq!(c.power_series, vec![180.0, 210.0, 230.0]);
    }

    #[test]
    fn should_mark_a_missing_reading_as_a_gap_in_the_series() {
        // 0.0 is the sparkline's "no reading", not a ride at zero watts.
        let priors = vec![effort(1, "Loop", None), effort(4, "Loop", Some(210))];
        let c = compare(&effort(9, "Loop", Some(230)), &priors).expect("two priors");
        assert_eq!(c.power_series, vec![0.0, 210.0, 230.0]);
    }

    // ── Indoor and outdoor ────────────────────────────────────────────────────

    fn outdoor(d: u32, name: &str, power: Option<u32>) -> Effort {
        Effort {
            indoor: false,
            ..effort(d, name, power)
        }
    }

    #[test]
    fn should_mark_the_apps_own_ride_indoor_even_when_its_twin_says_ride() {
        // Garmin may file an uploaded trainer ride under plain "Ride"; the app
        // knows better, because it has never recorded a ride on the road.
        let sessions = vec![summary(1, at(2), "Loop", Some(200.0), Some("i1"))];
        let activities = vec![activity("i1", 2, "Indoor Cycling", "Ride", Some(200))];
        let history = build_history(&sessions, &activities);
        assert_eq!(history.len(), 1);
        assert!(history[0].indoor);
    }

    #[test]
    fn should_take_an_imported_rides_setting_from_its_sport() {
        let activities = vec![
            activity("i1", 1, "Loop", "Ride", Some(180)),
            activity("i2", 2, "Loop", "VirtualRide", Some(190)),
        ];
        let history = build_history(&[], &activities);
        assert!(!history[0].indoor, "a road ride");
        assert!(history[1].indoor, "a virtual ride");
    }

    #[test]
    fn should_pick_the_last_ride_in_the_same_setting_past_a_newer_one_in_the_other() {
        let priors = vec![
            effort(1, "Loop", Some(180)),
            effort(3, "Loop", Some(185)),
            outdoor(6, "Loop", Some(210)),
        ];
        let c = compare(&effort(9, "Loop", Some(200)), &priors).expect("three priors");
        assert_eq!(c.previous.date, day(6), "power still sees the outdoor ride");
        assert_eq!(c.previous_like.expect("an indoor prior").date, day(3));
        assert_eq!(c.other_setting, 1);
    }

    #[test]
    fn should_have_no_like_for_like_ride_when_every_prior_was_outdoors() {
        let priors = vec![outdoor(1, "Loop", Some(180)), outdoor(4, "Loop", Some(190))];
        let c = compare(&effort(9, "Loop", Some(200)), &priors).expect("two priors");
        assert!(c.previous_like.is_none());
        assert_eq!(c.other_setting, 2);
    }

    #[test]
    fn should_match_an_outdoor_ride_with_outdoor_ones() {
        // The rule is symmetric, not "indoor rides only".
        let priors = vec![outdoor(1, "Loop", Some(180)), effort(4, "Loop", Some(190))];
        let c = compare(&outdoor(9, "Loop", Some(200)), &priors).expect("two priors");
        assert_eq!(c.previous_like.expect("an outdoor prior").date, day(1));
        assert_eq!(c.other_setting, 1);
    }

    #[test]
    fn should_still_rank_power_across_both_settings() {
        // Watts are watts: the outdoor record stands against the indoor ride.
        let priors = vec![outdoor(1, "Loop", Some(240)), effort(4, "Loop", Some(190))];
        let c = compare(&effort(9, "Loop", Some(200)), &priors).expect("two priors");
        assert_eq!(c.power_rank, Some(2));
        assert_eq!(c.power_series, vec![240.0, 190.0, 200.0]);
    }

    fn with_hr(e: Effort, hr: u32) -> Effort {
        Effort {
            average_hr: Some(hr),
            ..e
        }
    }

    fn over(e: Effort, metres: f32) -> Effort {
        Effort {
            distance_m: Some(metres),
            ..e
        }
    }

    #[test]
    fn should_never_set_indoor_efficiency_against_an_outdoor_ride() {
        let priors = vec![with_hr(outdoor(4, "Loop", Some(200)), 140)];
        let current = with_hr(effort(9, "Loop", Some(200)), 150);
        let c = compare(&current, &priors).expect("one prior");
        assert!(c.efficiency_baseline(&current).is_none());
    }

    #[test]
    fn should_skip_the_newer_outdoor_ride_for_efficiency() {
        let priors = vec![
            with_hr(effort(2, "Loop", Some(190)), 150),
            with_hr(outdoor(6, "Loop", Some(200)), 140),
        ];
        let current = with_hr(effort(9, "Loop", Some(200)), 150);
        let c = compare(&current, &priors).expect("two priors");
        assert_eq!(
            c.efficiency_baseline(&current)
                .expect("an indoor prior")
                .date,
            day(2)
        );
    }

    #[test]
    fn should_give_no_efficiency_baseline_when_this_ride_has_no_heart_rate() {
        let priors = vec![with_hr(effort(2, "Loop", Some(190)), 150)];
        let current = effort(9, "Loop", Some(200));
        let c = compare(&current, &priors).expect("one prior");
        assert!(c.efficiency_baseline(&current).is_none());
    }

    #[test]
    fn should_give_no_efficiency_baseline_when_the_like_ride_has_no_heart_rate() {
        let priors = vec![effort(2, "Loop", Some(190))];
        let current = with_hr(effort(9, "Loop", Some(200)), 150);
        let c = compare(&current, &priors).expect("one prior");
        assert!(c.efficiency_baseline(&current).is_none());
    }

    #[test]
    fn should_never_time_an_indoor_ride_against_an_outdoor_one_over_the_same_ground() {
        // The real case: the GPX of a road ride, replayed on the trainer.
        let priors = vec![over(outdoor(4, "Loop", Some(200)), 40_000.0)];
        let current = over(effort(9, "Loop", Some(200)), 40_000.0);
        let c = compare(&current, &priors).expect("one prior");
        assert!(c.time_baseline(&current).is_none());
    }

    #[test]
    fn should_time_against_the_last_ride_in_the_same_setting() {
        let priors = vec![
            over(effort(2, "Loop", Some(190)), 40_000.0),
            over(outdoor(6, "Loop", Some(200)), 40_000.0),
        ];
        let current = over(effort(9, "Loop", Some(200)), 40_000.0);
        let c = compare(&current, &priors).expect("two priors");
        assert_eq!(
            c.time_baseline(&current).expect("an indoor prior").date,
            day(2)
        );
    }

    #[test]
    fn should_still_refuse_a_time_over_different_ground_in_the_same_setting() {
        let priors = vec![over(effort(2, "Loop", Some(190)), 30_000.0)];
        let current = over(effort(9, "Loop", Some(200)), 40_000.0);
        let c = compare(&current, &priors).expect("one prior");
        assert!(c.time_baseline(&current).is_none());
    }

    #[test]
    fn should_not_time_against_a_ride_with_no_recorded_duration() {
        let mut prior = over(effort(2, "Loop", Some(190)), 40_000.0);
        prior.duration_secs = 0;
        let current = over(effort(9, "Loop", Some(200)), 40_000.0);
        let c = compare(&current, &[prior]).expect("one prior");
        assert!(c.time_baseline(&current).is_none());
    }
}
