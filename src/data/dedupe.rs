//! Recognising when a locally recorded ride and an Intervals.icu activity are the
//! same event.
//!
//! A ride recorded in the app can come back from Intervals.icu after making the
//! round trip through Garmin or Strava. Nothing survives that trip to identify it
//! by: Intervals.icu's `external_id` describes the Garmin file, not our export, and
//! no identifier of ours is guaranteed to be preserved. So the two are matched on
//! what the ride itself cannot change — when it started, how long it lasted, and
//! how far it went.
//!
//! The thresholds are deliberately tight. One athlete does not start two different
//! rides within minutes of each other, so a narrow window costs nothing real while
//! keeping false matches — which would hide a genuine ride — very unlikely.

use chrono::{DateTime, Local, NaiveDateTime, Utc};
use std::collections::HashMap;

use crate::data::db::IntervalsActivity;
use crate::data::session::Session;

/// How far apart two recordings of the same ride may claim to have started.
///
/// The app stamps the start when the rider begins pedalling and the head unit does
/// the same, so these agree closely; the allowance covers clock drift between
/// devices and Intervals.icu rounding to the minute.
const MAX_START_DIFF_SECS: i64 = 180;

/// Fractional duration difference allowed, on top of [`MIN_DURATION_SLACK_SECS`].
/// Moving time from Garmin excludes stops that the app's elapsed time includes.
const MAX_DURATION_DIFF_FRACTION: f64 = 0.10;

/// Absolute duration slack, so short rides are not rejected over a few seconds.
const MIN_DURATION_SLACK_SECS: f64 = 120.0;

/// Fractional distance difference allowed when both records report one.
const MAX_DISTANCE_DIFF_FRACTION: f32 = 0.10;

/// Whole-hour clock errors a match may carry, nearest first.
///
/// Rides the app recorded before 0.12.2 were exported with their local time
/// three hours out, and a file reaches Garmin with whatever time it carries.
/// A start that misses by exactly whole hours is taken as that error, but only
/// when the durations agree to [`MAX_OFFSET_DURATION_FRACTION`]: an hour's gap
/// alone is also what two separate rides look like.
const HOUR_OFFSETS: [i64; 6] = [1, -1, 2, -2, 3, -3];

/// How closely durations must agree for a match across a whole-hour offset.
const MAX_OFFSET_DURATION_FRACTION: f64 = 0.02;

/// How far apart the Intervals.icu and Garmin rows of one activity may claim
/// to have started. Both describe the same file, so they agree closely.
const MAX_TWIN_START_DIFF_SECS: i64 = 120;

/// Do `session` and `activity` describe the same ride?
///
/// Requires the activity to be a ride, to have started at essentially the same
/// moment, and to have lasted about as long. Distance is checked only when both
/// sides report it — an indoor ride without a speed sensor has none.
///
/// Test-only: the app matches through [`find_match`], which applies this rule
/// first and the whole-hour fallback after it.
#[cfg(test)]
pub fn is_same_activity(session: &Session, activity: &IntervalsActivity) -> bool {
    matches_shifted(session, activity, 0)
}

/// [`is_same_activity`], with the activity's clock taken to be `hours` out.
fn matches_shifted(session: &Session, activity: &IntervalsActivity, hours: i64) -> bool {
    if !is_ride(&activity.sport_type) {
        return false;
    }
    let Some(gap) = start_gap_secs(session, activity, hours) else {
        // Without a start time there is nothing precise enough to match on; the
        // date alone would pair any two rides on the same day.
        return false;
    };
    if gap > MAX_START_DIFF_SECS {
        return false;
    }
    let durations = if hours == 0 {
        durations_agree(session, activity)
    } else {
        durations_close(
            u32::try_from(session.duration_secs()).ok(),
            activity.duration_secs,
        )
    };
    durations && distances_agree(session, activity)
}

/// Seconds between the session's start and the activity's, once the activity's
/// clock is moved by `hours`.
fn start_gap_secs(session: &Session, activity: &IntervalsActivity, hours: i64) -> Option<i64> {
    let start = activity_start_utc(activity)? + chrono::Duration::hours(hours);
    Some((session.started_at - start).num_seconds().abs())
}

/// Find the activity that is the same ride as `session`, if any.
///
/// When several match — which the thresholds make very unlikely — the one starting
/// closest to the session wins. Only when nothing matches on the clock as it
/// stands are whole-hour errors tried, nearest first ([`HOUR_OFFSETS`]).
pub fn find_match<'a>(
    session: &Session,
    activities: impl IntoIterator<Item = &'a IntervalsActivity>,
) -> Option<&'a IntervalsActivity> {
    let activities: Vec<&IntervalsActivity> = activities.into_iter().collect();
    std::iter::once(0).chain(HOUR_OFFSETS).find_map(|hours| {
        activities
            .iter()
            .copied()
            .filter(|a| matches_shifted(session, a, hours))
            .min_by_key(|a| start_gap_secs(session, a, hours).unwrap_or(i64::MAX))
    })
}

/// Whether an activity id is one the Garmin sync wrote. Intervals.icu ids
/// start with `i`, so the two can never collide.
pub fn is_garmin_id(id: &str) -> bool {
    id.starts_with('g')
}

/// Pairs of `(intervals_id, garmin_id)` that are one activity held twice: once
/// as Intervals.icu synced it before Garmin became the source, once from Garmin.
///
/// Twins are the same sport and started within [`MAX_TWIN_START_DIFF_SECS`] of
/// each other — or of a whole-hour clock error ([`HOUR_OFFSETS`]) when their
/// moving times also agree to [`MAX_OFFSET_DURATION_FRACTION`]. Each
/// Intervals.icu row pairs with at most one Garmin row, the nearest, preferring
/// no offset over any; several Intervals.icu rows may pair with one Garmin row,
/// since a ride uploaded there twice is still one file at Garmin.
pub fn find_twins(activities: &[IntervalsActivity]) -> Vec<(String, String)> {
    let garmin: Vec<&IntervalsActivity> = activities
        .iter()
        .filter(|a| is_garmin_id(&a.icu_id))
        .collect();
    activities
        .iter()
        .filter(|a| !is_garmin_id(&a.icu_id))
        .filter_map(|icu| {
            let (_, twin) = garmin
                .iter()
                .filter_map(|g| Some((twin_rank(icu, g)?, *g)))
                .min_by_key(|(rank, _)| *rank)?;
            Some((icu.icu_id.clone(), twin.icu_id.clone()))
        })
        .collect()
}

/// Twins found through a ride recorded here, as `(intervals_id, garmin_id)`:
/// an Intervals.icu row a session is linked to, and an unclaimed Garmin row
/// that [`find_match`] takes for the same ride as that session.
///
/// This catches what [`find_twins`] cannot. Intervals.icu may keep the moving
/// time where Garmin keeps the whole ride, and a whole-hour clock error then
/// leaves the two rows too far apart to trust each other — but the session is
/// a third clock and a third duration, and the Garmin row can agree with it.
pub fn find_session_twins(
    sessions: &[&Session],
    activities: &[IntervalsActivity],
) -> Vec<(String, String)> {
    let claimed: std::collections::HashSet<&str> = sessions
        .iter()
        .filter_map(|s| s.icu_id.as_deref())
        .collect();
    let mut taken = std::collections::HashSet::new();
    let mut pairs = Vec::new();
    for session in sessions {
        let Some(icu) = session.icu_id.as_deref().filter(|id| !is_garmin_id(id)) else {
            continue;
        };
        let candidates = activities.iter().filter(|a| {
            is_garmin_id(&a.icu_id)
                && !claimed.contains(a.icu_id.as_str())
                && !taken.contains(&a.icu_id)
        });
        if let Some(g) = find_match(session, candidates) {
            taken.insert(g.icu_id.clone());
            pairs.push((icu.to_string(), g.icu_id.clone()));
        }
    }
    pairs
}

/// How near a twin `g` is to `icu` as (hours of clock error, seconds left
/// over), or `None` when it is not a twin at all.
fn twin_rank(icu: &IntervalsActivity, g: &IntervalsActivity) -> Option<(i64, i64)> {
    let class = |a: &IntervalsActivity| crate::data::sport::normalize_sport_type(&a.sport_type);
    if class(icu).to_lowercase() != class(g).to_lowercase() {
        return None;
    }
    let gap = (g.start_datetime_local? - icu.start_datetime_local?).num_seconds();
    if gap.abs() <= MAX_TWIN_START_DIFF_SECS {
        return Some((0, gap.abs()));
    }
    let hours = HOUR_OFFSETS
        .into_iter()
        .find(|h| (gap - h * 3600).abs() <= MAX_TWIN_START_DIFF_SECS)?;
    if !durations_close(icu.duration_secs, g.duration_secs) {
        return None;
    }
    Some((hours.abs(), (gap - hours * 3600).abs()))
}

/// Whether two durations agree to [`MAX_OFFSET_DURATION_FRACTION`]. Unknown
/// or zero on either side is not agreement: it is the one check standing
/// between a clock error and two different rides.
fn durations_close(a: Option<u32>, b: Option<u32>) -> bool {
    let (Some(a), Some(b)) = (a, b) else {
        return false;
    };
    if a == 0 || b == 0 {
        return false;
    }
    let (a, b) = (f64::from(a), f64::from(b));
    (a - b).abs() <= a.max(b) * MAX_OFFSET_DURATION_FRACTION
}

/// What makes two Intervals.icu rows the same upload: same sport, started at the
/// same wall-clock moment, lasted the same time.
type DuplicateKey = (String, NaiveDateTime, Option<u32>);

/// The key an activity groups under, or `None` when it cannot be grouped safely.
///
/// Unlike matching a session to an activity, there is no clock drift to allow
/// for here — a duplicate is the *same file* reaching Intervals.icu twice, so
/// the two agree to the second. Exact keys keep the collapse from ever merging
/// two rides that merely started close together.
///
/// An activity with no start time is left ungrouped: the date alone would pair
/// any two rides on the same day.
fn duplicate_key(activity: &IntervalsActivity) -> Option<DuplicateKey> {
    Some((
        activity.sport_type.clone(),
        activity.start_datetime_local?,
        activity.duration_secs,
    ))
}

/// Drop repeated uploads of the same activity, keeping one of each.
///
/// The same ride reaches Intervals.icu more than once when it is uploaded again
/// — re-exporting a FIT file, or syncing from two sources. Each upload gets its
/// own `icu_id`, so nothing downstream recognises them as one ride: the activity
/// shows up several times in the calendar, and, worse, its TSS is counted once
/// per copy, inflating fitness and fatigue.
///
/// `prefer` marks the copy to keep when there is a choice. Callers that go on to
/// filter out session-linked activities pass their linked set, so the survivor is
/// the copy the link removes — otherwise the ride would appear twice, once as the
/// local session and once as the surviving orphan.
///
/// Input order is preserved.
pub fn collapse_duplicates(
    activities: Vec<IntervalsActivity>,
    prefer: impl Fn(&IntervalsActivity) -> bool,
) -> Vec<IntervalsActivity> {
    let mut kept: Vec<IntervalsActivity> = Vec::with_capacity(activities.len());
    let mut seen: HashMap<DuplicateKey, usize> = HashMap::new();

    for activity in activities {
        let Some(key) = duplicate_key(&activity) else {
            kept.push(activity);
            continue;
        };
        match seen.get(&key) {
            Some(&i) => {
                if prefer(&activity) && !prefer(&kept[i]) {
                    kept[i] = activity;
                }
            }
            None => {
                seen.insert(key, kept.len());
                kept.push(activity);
            }
        }
    }

    kept
}

/// Intervals.icu reports local wall-clock time with no offset, so it is read back
/// in the machine's current timezone — the one the ride was recorded in.
fn activity_start_utc(activity: &IntervalsActivity) -> Option<DateTime<Utc>> {
    let naive = activity.start_datetime_local?;
    naive
        .and_local_timezone(Local)
        .earliest()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Cycling activity types, as Intervals.icu names them.
fn is_ride(sport_type: &str) -> bool {
    matches!(
        sport_type,
        "Ride" | "VirtualRide" | "EBikeRide" | "Velomobile"
    )
}

fn durations_agree(session: &Session, activity: &IntervalsActivity) -> bool {
    let Some(activity_secs) = activity.duration_secs else {
        return true; // nothing to contradict the start-time match
    };
    let session_secs = session.duration_secs() as f64;
    let activity_secs = activity_secs as f64;
    let allowed =
        (session_secs.max(activity_secs) * MAX_DURATION_DIFF_FRACTION).max(MIN_DURATION_SLACK_SECS);
    (session_secs - activity_secs).abs() <= allowed
}

fn distances_agree(session: &Session, activity: &IntervalsActivity) -> bool {
    let Some(activity_m) = activity.distance_m else {
        return true;
    };
    let session_m = session.distance_m();
    // A ride recorded without a speed sensor has no distance of its own to compare.
    if session_m <= 0.0 || activity_m <= 0.0 {
        return true;
    }
    let allowed = session_m.max(activity_m) * MAX_DISTANCE_DIFF_FRACTION;
    (session_m - activity_m).abs() <= allowed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::session::DataPoint;
    use chrono::{Duration, NaiveDateTime};

    /// An activity as Intervals.icu would report `session` after a round trip.
    fn activity_for(session: &Session, icu_id: &str) -> IntervalsActivity {
        let local = session.started_at.with_timezone(&Local);
        IntervalsActivity {
            icu_id: icu_id.into(),
            date: local.date_naive(),
            name: "Morning Ride".into(),
            tss: Some(80.0),
            duration_secs: Some(session.duration_secs() as u32),
            average_watts: Some(200),
            normalized_watts: Some(210),
            average_hr: Some(140),
            max_hr: Some(170),
            sport_type: "VirtualRide".into(),
            start_datetime_local: Some(local.naive_local()),
            distance_m: Some(session.distance_m()),
            elevation_gain_m: Some(100.0),
            average_cadence: Some(85.0),
        }
    }

    /// A one-hour ride at a steady 30 km/h, so distance is 30 km.
    fn ride() -> Session {
        let mut s = Session::new(None);
        s.started_at = Utc::now() - Duration::hours(2);
        s.ended_at = Some(s.started_at + Duration::hours(1));
        s.data_points = (0..3600)
            .map(|i| DataPoint {
                elapsed_secs: i,
                power_watts: Some(200),
                target_watts: None,
                heart_rate_bpm: Some(140),
                cadence_rpm: Some(85),
                speed_kmh: Some(30.0),
                lat: None,
                lng: None,
                altitude_m: None,
            })
            .collect();
        s
    }

    fn shift_start(activity: &mut IntervalsActivity, secs: i64) {
        let start = activity
            .start_datetime_local
            .expect("test activity has a start");
        activity.start_datetime_local = Some(start + Duration::seconds(secs));
    }

    #[test]
    fn should_match_the_same_ride_returned_by_intervals() {
        let session = ride();
        let activity = activity_for(&session, "i1");
        assert!(is_same_activity(&session, &activity));
    }

    #[test]
    fn should_match_despite_a_small_clock_difference() {
        let session = ride();
        let mut activity = activity_for(&session, "i1");
        shift_start(&mut activity, 90);
        assert!(is_same_activity(&session, &activity));
    }

    #[test]
    fn should_not_match_a_ride_starting_much_later() {
        let session = ride();
        let mut activity = activity_for(&session, "i1");
        shift_start(&mut activity, 1800); // half an hour later — a different ride
        assert!(!is_same_activity(&session, &activity));
    }

    #[test]
    fn should_not_match_a_ride_of_a_very_different_length() {
        let session = ride();
        let mut activity = activity_for(&session, "i1");
        activity.duration_secs = Some(3600 * 3);
        assert!(!is_same_activity(&session, &activity));
    }

    #[test]
    fn should_tolerate_moving_time_being_shorter_than_elapsed() {
        // Garmin reports moving time, which excludes a couple of minutes stopped.
        let session = ride();
        let mut activity = activity_for(&session, "i1");
        activity.duration_secs = Some(3480);
        assert!(is_same_activity(&session, &activity));
    }

    #[test]
    fn should_not_match_a_different_sport() {
        let session = ride();
        let mut activity = activity_for(&session, "i1");
        activity.sport_type = "Run".into();
        assert!(!is_same_activity(&session, &activity));
    }

    #[test]
    fn should_not_match_a_very_different_distance() {
        let session = ride();
        let mut activity = activity_for(&session, "i1");
        activity.distance_m = Some(5_000.0);
        assert!(!is_same_activity(&session, &activity));
    }

    #[test]
    fn should_match_when_the_indoor_ride_recorded_no_distance() {
        let mut session = ride();
        for p in &mut session.data_points {
            p.speed_kmh = None;
        }
        let mut activity = activity_for(&session, "i1");
        activity.distance_m = Some(30_000.0);
        assert!(is_same_activity(&session, &activity));
    }

    #[test]
    fn should_not_match_without_a_start_time() {
        let session = ride();
        let mut activity = activity_for(&session, "i1");
        activity.start_datetime_local = None;
        assert!(
            !is_same_activity(&session, &activity),
            "a date alone would pair any two rides on the same day"
        );
    }

    #[test]
    fn should_pick_the_closest_of_several_candidates() {
        let session = ride();
        let mut near = activity_for(&session, "near");
        shift_start(&mut near, 20);
        let mut far = activity_for(&session, "far");
        shift_start(&mut far, 150);
        let unrelated = IntervalsActivity {
            start_datetime_local: Some(
                NaiveDateTime::parse_from_str("2020-01-01 08:00:00", "%Y-%m-%d %H:%M:%S")
                    .expect("hardcoded valid datetime"),
            ),
            ..activity_for(&session, "unrelated")
        };
        let found = find_match(&session, [&far, &near, &unrelated]).expect("a match");
        assert_eq!(found.icu_id, "near");
    }

    #[test]
    fn should_find_nothing_when_no_activity_matches() {
        let session = ride();
        let mut activity = activity_for(&session, "i1");
        // Not a whole number of hours out: no clock error explains it. (Exactly
        // two hours, with the durations agreeing, is one — see below.)
        shift_start(&mut activity, 7200 + 600);
        assert!(find_match(&session, [&activity]).is_none());
    }

    // ── Collapsing repeated uploads ───────────────────────────────────────

    /// Nothing is preferred — the first copy of each ride survives.
    fn collapse(activities: Vec<IntervalsActivity>) -> Vec<String> {
        collapse_duplicates(activities, |_| false)
            .into_iter()
            .map(|a| a.icu_id)
            .collect()
    }

    /// The same ride uploaded again: a new id, everything else identical.
    fn reupload(activity: &IntervalsActivity, icu_id: &str) -> IntervalsActivity {
        IntervalsActivity {
            icu_id: icu_id.into(),
            ..activity.clone()
        }
    }

    #[test]
    fn should_keep_a_ride_uploaded_only_once() {
        let session = ride();
        let activity = activity_for(&session, "i1");
        assert_eq!(collapse(vec![activity]), vec!["i1"]);
    }

    #[test]
    fn should_collapse_a_ride_uploaded_three_times() {
        let session = ride();
        let first = activity_for(&session, "i1");
        let copies = vec![reupload(&first, "i2"), reupload(&first, "i3"), first];
        assert_eq!(collapse(copies), vec!["i2"]);
    }

    #[test]
    fn should_keep_the_copy_a_session_already_points_at() {
        // The link is what removes the ride from the unlinked view, so the
        // linked copy has to be the survivor or the ride shows up twice.
        let session = ride();
        let first = activity_for(&session, "i1");
        let copies = vec![first.clone(), reupload(&first, "i2")];
        let kept = collapse_duplicates(copies, |a| a.icu_id == "i2");
        assert_eq!(
            kept.into_iter().map(|a| a.icu_id).collect::<Vec<_>>(),
            vec!["i2"]
        );
    }

    #[test]
    fn should_keep_two_different_rides_on_the_same_day() {
        let morning = ride();
        let first = activity_for(&morning, "i1");
        let mut evening = reupload(&first, "i2");
        shift_start(&mut evening, 36_000); // ten hours later
        assert_eq!(collapse(vec![first, evening]), vec!["i1", "i2"]);
    }

    #[test]
    fn should_keep_rides_that_started_together_but_ran_different_lengths() {
        // Same trainer, same minute, different ride — not one upload twice.
        let session = ride();
        let first = activity_for(&session, "i1");
        let mut longer = reupload(&first, "i2");
        longer.duration_secs = Some(first.duration_secs.expect("set above") + 600);
        assert_eq!(collapse(vec![first, longer]), vec!["i1", "i2"]);
    }

    #[test]
    fn should_keep_a_ride_and_a_run_recorded_together() {
        // A brick session, or a watch recording alongside the head unit.
        let session = ride();
        let cycling = activity_for(&session, "i1");
        let mut running = reupload(&cycling, "i2");
        running.sport_type = "Run".into();
        assert_eq!(collapse(vec![cycling, running]), vec!["i1", "i2"]);
    }

    #[test]
    fn should_not_group_activities_that_have_no_start_time() {
        // Without a start there is nothing precise enough to group on, so these
        // are left alone rather than merged on their date.
        let session = ride();
        let first = activity_for(&session, "i1");
        let mut a = reupload(&first, "i1");
        let mut b = reupload(&first, "i2");
        a.start_datetime_local = None;
        b.start_datetime_local = None;
        assert_eq!(collapse(vec![a, b]), vec!["i1", "i2"]);
    }

    #[test]
    fn should_leave_an_empty_history_alone() {
        assert!(collapse(vec![]).is_empty());
    }

    #[test]
    fn should_preserve_the_order_rides_came_in() {
        let session = ride();
        let first = activity_for(&session, "i1");
        let mut second = reupload(&first, "i2");
        shift_start(&mut second, 3600);
        let mut third = reupload(&first, "i3");
        shift_start(&mut third, 7200);
        let jumbled = vec![
            first.clone(),
            second.clone(),
            reupload(&first, "i1-again"),
            third,
        ];
        assert_eq!(collapse(jumbled), vec!["i1", "i2", "i3"]);
    }

    // ── Whole-hour clock errors ──

    #[test]
    fn should_match_a_ride_whose_clock_is_three_hours_out_when_durations_agree() {
        let session = ride();
        let mut activity = activity_for(&session, "g1");
        shift_start(&mut activity, -3 * 3600 + 60);
        // 2 % of an hour is 72 s.
        activity.duration_secs = Some(3600 - 72);
        assert!(!is_same_activity(&session, &activity));
        assert_eq!(
            find_match(&session, [&activity]).map(|a| a.icu_id.as_str()),
            Some("g1")
        );
    }

    #[test]
    fn should_not_match_across_an_hour_when_durations_differ_by_more_than_two_percent() {
        let session = ride();
        let mut activity = activity_for(&session, "g1");
        shift_start(&mut activity, 3600);
        // Inside the ±10 % an exact-clock match allows, outside the 2 % here.
        activity.duration_secs = Some(3600 - 73);
        assert!(find_match(&session, [&activity]).is_none());
        // And never with no duration to compare at all.
        activity.duration_secs = None;
        assert!(find_match(&session, [&activity]).is_none());
    }

    #[test]
    fn should_not_match_four_hours_out() {
        let session = ride();
        let mut activity = activity_for(&session, "g1");
        shift_start(&mut activity, 4 * 3600);
        assert!(find_match(&session, [&activity]).is_none());
    }

    #[test]
    fn should_prefer_the_ride_on_time_over_one_an_hour_out() {
        let session = ride();
        let mut early = activity_for(&session, "g-hour");
        shift_start(&mut early, -3600);
        let mut on_time = activity_for(&session, "g-late");
        // Late by the full allowance, still preferred to an exact hour's error.
        shift_start(&mut on_time, MAX_START_DIFF_SECS);
        assert_eq!(
            find_match(&session, [&early, &on_time]).map(|a| a.icu_id.as_str()),
            Some("g-late")
        );
    }

    // ── Twins ──

    fn twin(id: &str, sport: &str, start: NaiveDateTime, secs: u32) -> IntervalsActivity {
        IntervalsActivity {
            icu_id: id.into(),
            date: start.date(),
            name: String::new(),
            tss: None,
            duration_secs: Some(secs),
            average_watts: None,
            normalized_watts: None,
            average_hr: None,
            max_hr: None,
            sport_type: sport.into(),
            start_datetime_local: Some(start),
            distance_m: None,
            elevation_gain_m: None,
            average_cadence: None,
        }
    }

    fn at(h: u32, m: u32, sec: u32) -> NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 9, 14)
            .and_then(|d| d.and_hms_opt(h, m, sec))
            .expect("hardcoded valid time")
    }

    #[test]
    fn should_pair_an_icu_row_with_the_garmin_row_of_the_same_file() {
        let rows = [
            twin("i1", "Ride", at(8, 0, 0), 3600),
            twin("g1", "Ride", at(8, 2, 0), 3000),
        ];
        // 120 s apart pairs whatever the durations say; the clocks agree.
        assert_eq!(find_twins(&rows), [("i1".into(), "g1".into())]);
    }

    #[test]
    fn should_not_pair_rows_121_seconds_apart() {
        let rows = [
            twin("i1", "Ride", at(8, 0, 0), 3600),
            twin("g1", "Ride", at(8, 2, 1), 3600),
        ];
        assert!(find_twins(&rows).is_empty());
    }

    #[test]
    fn should_pair_across_a_whole_hour_only_when_moving_times_agree() {
        let icu = twin("i1", "VirtualRide", at(8, 0, 0), 3600);
        let close = twin("g1", "VirtualRide", at(11, 2, 0), 3600 - 72);
        assert_eq!(
            find_twins(&[icu.clone(), close]),
            [("i1".into(), "g1".into())]
        );
        let far = twin("g1", "VirtualRide", at(11, 0, 0), 3600 - 73);
        assert!(find_twins(&[icu.clone(), far]).is_empty());
        let late = twin("g1", "VirtualRide", at(11, 2, 1), 3600);
        assert!(find_twins(&[icu.clone(), late]).is_empty());
        let four = twin("g1", "VirtualRide", at(12, 0, 0), 3600);
        assert!(find_twins(&[icu, four]).is_empty());
    }

    #[test]
    fn should_pair_only_within_one_sport_class_however_the_services_spell_it() {
        // Intervals.icu files a gravel ride as a ride; Garmin as gravel.
        let rows = [
            twin("i1", "Ride", at(8, 0, 0), 3600),
            twin("g1", "GravelRide", at(8, 0, 30), 3600),
            twin("i2", "Run", at(18, 0, 0), 1800),
            twin("g2", "Walk", at(18, 0, 0), 1800),
        ];
        assert_eq!(find_twins(&rows), [("i1".into(), "g1".into())]);
    }

    #[test]
    fn should_pair_each_icu_copy_of_one_upload_with_the_one_garmin_row() {
        let rows = [
            twin("i1", "Ride", at(8, 0, 0), 3600),
            twin("i2", "Ride", at(8, 0, 0), 3600),
            twin("g1", "Ride", at(8, 0, 5), 3600),
        ];
        assert_eq!(
            find_twins(&rows),
            [("i1".into(), "g1".into()), ("i2".into(), "g1".into())]
        );
    }

    #[test]
    fn should_pick_the_nearest_garmin_row_and_never_pair_garmin_with_garmin() {
        let rows = [
            twin("g-far", "Ride", at(8, 1, 30), 3600),
            twin("i1", "Ride", at(8, 0, 0), 3600),
            twin("g-near", "Ride", at(7, 59, 50), 3600),
        ];
        assert_eq!(find_twins(&rows), [("i1".into(), "g-near".into())]);
    }

    #[test]
    fn should_leave_a_row_with_no_start_time_unpaired() {
        let mut icu = twin("i1", "Ride", at(8, 0, 0), 3600);
        icu.start_datetime_local = None;
        assert!(find_twins(&[icu, twin("g1", "Ride", at(8, 0, 0), 3600)]).is_empty());
    }

    #[test]
    fn should_never_take_a_garmin_row_another_session_already_claims() {
        let mut linked_to_icu = ride();
        linked_to_icu.icu_id = Some("i1".into());
        let mut linked_to_garmin = ride();
        linked_to_garmin.icu_id = Some("g1".into());
        let g = activity_for(&linked_to_icu, "g1");
        assert!(find_session_twins(&[&linked_to_icu, &linked_to_garmin], &[g]).is_empty());
    }

    #[test]
    fn should_fold_one_garmin_row_into_one_icu_row_only() {
        // Two sessions on one ride, each linked to its own icu copy.
        let mut a = ride();
        a.icu_id = Some("i1".into());
        let mut b = a.clone();
        b.icu_id = Some("i2".into());
        let g = activity_for(&a, "g1");
        assert_eq!(
            find_session_twins(&[&a, &b], &[g]),
            [("i1".to_string(), "g1".to_string())]
        );
    }

    #[test]
    fn should_ignore_sessions_unlinked_or_already_linked_to_garmin() {
        let unlinked = ride();
        let g = activity_for(&unlinked, "g1");
        assert!(find_session_twins(&[&unlinked], std::slice::from_ref(&g)).is_empty());
        let mut on_garmin = ride();
        on_garmin.icu_id = Some("g9".into());
        assert!(find_session_twins(&[&on_garmin], &[g]).is_empty());
    }

    #[test]
    fn should_pick_the_nearer_twin_across_a_negative_whole_hour_offset() {
        let rows = [
            twin("i1", "Ride", at(11, 0, 0), 3600),
            twin("g-far", "Ride", at(8, 1, 40), 3600),
            twin("g-near", "Ride", at(7, 59, 50), 3600),
        ];
        assert_eq!(find_twins(&rows), [("i1".into(), "g-near".into())]);
    }
}
