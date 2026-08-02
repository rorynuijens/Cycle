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

use chrono::{DateTime, Local, Utc};

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
}
