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

/// Do `session` and `activity` describe the same ride?
///
/// Requires the activity to be a ride, to have started at essentially the same
/// moment, and to have lasted about as long. Distance is checked only when both
/// sides report it — an indoor ride without a speed sensor has none.
pub fn is_same_activity(session: &Session, activity: &IntervalsActivity) -> bool {
    if !is_ride(&activity.sport_type) {
        return false;
    }
    let Some(activity_start) = activity_start_utc(activity) else {
        // Without a start time there is nothing precise enough to match on; the
        // date alone would pair any two rides on the same day.
        return false;
    };
    if (session.started_at - activity_start).num_seconds().abs() > MAX_START_DIFF_SECS {
        return false;
    }
    if !durations_agree(session, activity) {
        return false;
    }
    distances_agree(session, activity)
}

/// Slack allowed at either edge of a legacy session's recorded span.
const LEGACY_EDGE_SLACK_SECS: i64 = 180;

/// Do `session` and `activity` describe the same ride, judged the way rides
/// recorded before the start-time fix have to be judged?
///
/// Those sessions were stamped when the workout was selected or the route page
/// opened rather than at the first pedal stroke, so their start can be arbitrarily
/// early and their duration inflated by the same amount. Both errors run in one
/// direction only, and `ended_at` was always correct — so rather than widening the
/// window symmetrically, the real ride is asked to *fit inside* the recorded span:
/// it must have started within it and cannot have lasted longer than it.
///
/// Used only by the one-off backfill in [`crate::data::db::backfill_icu_links`].
pub fn is_same_activity_legacy(session: &Session, activity: &IntervalsActivity) -> bool {
    if !is_ride(&activity.sport_type) {
        return false;
    }
    let Some(activity_start) = activity_start_utc(activity) else {
        return false;
    };
    let Some(session_end) = session.ended_at else {
        return false; // an unfinished session has no span to contain anything
    };

    let starts_within_span = activity_start
        >= session.started_at - chrono::Duration::seconds(LEGACY_EDGE_SLACK_SECS)
        && activity_start <= session_end + chrono::Duration::seconds(LEGACY_EDGE_SLACK_SECS);
    if !starts_within_span {
        return false;
    }

    // The real ride cannot have lasted longer than the span that supposedly
    // contains it, give or take the same slack.
    if let Some(activity_secs) = activity.duration_secs {
        let span_secs = session.duration_secs() as i64;
        if activity_secs as i64 > span_secs + LEGACY_EDGE_SLACK_SECS {
            return false;
        }
    }

    distances_agree(session, activity)
}

/// Find the activity that is the same ride as `session`, if any.
///
/// When several match — which the thresholds make very unlikely — the one starting
/// closest to the session wins.
pub fn find_match<'a>(
    session: &Session,
    activities: impl IntoIterator<Item = &'a IntervalsActivity>,
) -> Option<&'a IntervalsActivity> {
    find_match_with(session, activities, is_same_activity)
}

/// As [`find_match`], but with the caller choosing how sameness is judged — the
/// one-off backfill uses [`is_same_activity_legacy`].
pub fn find_match_with<'a>(
    session: &Session,
    activities: impl IntoIterator<Item = &'a IntervalsActivity>,
    same: impl Fn(&Session, &IntervalsActivity) -> bool,
) -> Option<&'a IntervalsActivity> {
    activities
        .into_iter()
        .filter(|a| same(session, a))
        .min_by_key(|a| {
            activity_start_utc(a)
                .map(|s| (session.started_at - s).num_seconds().abs())
                .unwrap_or(i64::MAX)
        })
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

    // ── Legacy matching (rides recorded before the start-time fix) ──────────

    /// A ride as it was recorded before the fix: the session was stamped 40
    /// minutes before the rider actually started, so its span is inflated at the
    /// front while its end is correct.
    fn legacy_ride() -> Session {
        let mut s = ride();
        s.started_at -= Duration::minutes(40);
        s
    }

    #[test]
    fn legacy_matching_tolerates_an_inflated_start() {
        let session = legacy_ride();
        // Intervals.icu has the real ride: it began 40 minutes into the span.
        let real_start = session.ended_at.expect("ride has an end") - Duration::hours(1);
        let mut activity = activity_for(&session, "i1");
        activity.start_datetime_local = Some(real_start.with_timezone(&Local).naive_local());
        activity.duration_secs = Some(3600);

        assert!(
            !is_same_activity(&session, &activity),
            "the everyday matcher is right to reject a 40-minute start difference"
        );
        assert!(is_same_activity_legacy(&session, &activity));
    }

    #[test]
    fn legacy_matching_rejects_an_activity_outside_the_span() {
        let session = legacy_ride();
        let mut activity = activity_for(&session, "i1");
        let after_end = session.ended_at.expect("ride has an end") + Duration::hours(2);
        activity.start_datetime_local = Some(after_end.with_timezone(&Local).naive_local());
        assert!(!is_same_activity_legacy(&session, &activity));
    }

    #[test]
    fn legacy_matching_rejects_a_ride_longer_than_the_span_that_holds_it() {
        let session = legacy_ride(); // span is 1 h 40 m
        let mut activity = activity_for(&session, "i1");
        activity.duration_secs = Some(3600 * 3);
        assert!(!is_same_activity_legacy(&session, &activity));
    }

    #[test]
    fn legacy_matching_still_rejects_a_different_sport() {
        let session = legacy_ride();
        let mut activity = activity_for(&session, "i1");
        activity.sport_type = "Run".into();
        assert!(!is_same_activity_legacy(&session, &activity));
    }

    #[test]
    fn legacy_matching_needs_a_finished_session() {
        let mut session = legacy_ride();
        session.ended_at = None;
        let activity = activity_for(&session, "i1");
        assert!(!is_same_activity_legacy(&session, &activity));
    }

    #[test]
    fn should_find_nothing_when_no_activity_matches() {
        let session = ride();
        let mut activity = activity_for(&session, "i1");
        shift_start(&mut activity, 7200);
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
}
