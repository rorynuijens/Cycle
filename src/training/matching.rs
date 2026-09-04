//! What the rider actually rode, on the days the plan asked for something.
//!
//! Until 0.6.0 a planned day could only be closed by riding that exact workout,
//! in this app, on that day. A rider who trains mostly outdoors therefore read as
//! missing session after session — twelve of them here, eight of which were real
//! and four of which were 90-minute road rides the app was holding all along.
//! Those four were not a missing button. They were the program asking the wrong
//! question.
//!
//! So this module answers "was there real training on this day?", and the program
//! reads it directly ([`trained_days`]). Nothing is asked of the rider: the app
//! already has the ride, and making them confirm it — by naming an outdoor ride
//! as the interval session it plainly was not — was a fiction invented to satisfy
//! a flag with only two states.
//!
//! Everything here is local, deterministic and free. No AI call.

use chrono::NaiveDate;

use crate::data::calendar::CalendarEvent;
use crate::data::sport::is_cycling;

/// The shortest ride that counts as having trained, in seconds.
///
/// This is the *only* duration rule, and it is a floor rather than a band.
///
/// A band comparing the ride against the planned session was the obvious design
/// and is wrong for this rider: against planned sessions of 30–65 minutes their
/// real rides run 18 to 89 minutes, giving ratios from 0.30 to 2.97. A
/// 0.5×–1.5× window would have called almost every one of their real training
/// days a missed session. Riding *more* than the plan asked is not a reason to
/// pretend the day was missed.
///
/// Ten minutes only keeps out a trainer left recording by accident.
pub const MIN_COUNTABLE_SECS: u32 = 600;

/// A ride the rider did on a given day.
#[derive(Debug, Clone, PartialEq)]
pub struct DayRide {
    pub name: String,
    pub duration_secs: u32,
    /// `None` when the ride carries no load figure — an Intervals.icu activity
    /// synced without one, or a session with no power to score.
    pub tss: Option<f32>,
}

impl DayRide {
    /// "88 min · 106 TSS", or just the minutes when there is no load figure.
    pub fn summary(&self) -> String {
        let mins = self.duration_secs / 60;
        match self.tss {
            Some(tss) => format!("{mins} min · {tss:.0} TSS"),
            None => format!("{mins} min"),
        }
    }
}

/// The rides done on `date`, biggest first.
///
/// `events` is one day's worth of the calendar's own timeline, so this reuses
/// what the page has already loaded rather than querying again; anything not on
/// `date` is ignored, which makes the day the caller means explicit rather than
/// implied by how they sliced the list.
///
/// Two sources produce rides and the rest are skipped: a planned entry is
/// something asked for, not something done, and a day off is not a ride. The
/// activities must come from
/// [`crate::data::db::load_unlinked_intervals_activities_between`] — a ride this
/// app recorded and then uploaded exists in *both* sources, and the unlinked
/// loader is what stops it being listed twice.
///
/// Ordering is by training load, then by length, then by name, so a day holding
/// both a 21-minute spin and an 88-minute road ride names the road ride first,
/// and always in the same order.
pub fn rides_on<'a>(
    date: NaiveDate,
    events: impl IntoIterator<Item = &'a CalendarEvent>,
    fallback_ftp: u32,
) -> Vec<DayRide> {
    let mut rides: Vec<DayRide> = events
        .into_iter()
        .filter(|e| e.date() == Some(date))
        .filter_map(|event| match event {
            CalendarEvent::Session(record, workout_name) => {
                let session = &record.session;
                Some(DayRide {
                    // The rider's own title first, then the workout it was ridden
                    // from: Garmin round-trips leave indoor rides untitled, and
                    // "Endurance 60" beats offering them an unnamed ride.
                    name: session
                        .title
                        .clone()
                        .or_else(|| workout_name.clone())
                        .unwrap_or_else(|| "Recorded ride".to_string()),
                    duration_secs: session.duration_secs() as u32,
                    tss: session.tss(fallback_ftp),
                })
            }
            CalendarEvent::IcuActivity(activity) => {
                // A session has no sport field — one recorded here is always a
                // ride — but a synced activity can be anything the rider does.
                if !is_cycling(&activity.sport_type) {
                    return None;
                }
                Some(DayRide {
                    name: activity.name.clone(),
                    duration_secs: activity.duration_secs.unwrap_or(0),
                    tss: activity.tss,
                })
            }
            CalendarEvent::Scheduled(_) | CalendarEvent::TimeOff(_) => None,
        })
        .filter(|c| c.duration_secs >= MIN_COUNTABLE_SECS)
        .collect();

    rides.sort_by(|a, b| {
        b.tss
            .unwrap_or(0.0)
            .total_cmp(&a.tss.unwrap_or(0.0))
            .then(b.duration_secs.cmp(&a.duration_secs))
            .then(a.name.cmp(&b.name))
    });
    rides
}

/// Every day real training happened on.
///
/// This is what makes a planned day the rider rode through stop reading as
/// missed, without anyone having to say so. The program compares its planned
/// dates against this set (`training::program::status`); a day in it was trained
/// on, whatever the plan had asked for that day.
///
/// The same two rules as [`rides_on`], for the same reasons: cycling only, and
/// at least [`MIN_COUNTABLE_SECS`]. `activities` must be the unlinked set, though
/// here a double-counted twin would be harmless — it is a set of dates, not a
/// tally.
pub fn trained_days(
    sessions: &[crate::data::db::SessionSummary],
    activities: &[crate::data::db::IntervalsActivity],
) -> std::collections::HashSet<NaiveDate> {
    let mut days = std::collections::HashSet::new();
    for s in sessions {
        if s.duration_secs >= MIN_COUNTABLE_SECS as u64 {
            days.insert(s.started_at.with_timezone(&chrono::Local).date_naive());
        }
    }
    for a in activities {
        if is_cycling(&a.sport_type) && a.duration_secs.unwrap_or(0) >= MIN_COUNTABLE_SECS {
            days.insert(a.date);
        }
    }
    days
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::db::{IntervalsActivity, SessionRecord, TimeOffEntry};
    use crate::data::session::Session;
    use chrono::{TimeZone, Utc};

    const FTP: u32 = 211;

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 8, 6).expect("hardcoded valid date")
    }

    /// A recorded ride of `mins` minutes, starting mid-morning local time.
    fn session(id: i64, title: Option<&str>, mins: i64) -> CalendarEvent {
        let started = Utc
            .with_ymd_and_hms(2026, 8, 6, 7, 0, 0)
            .single()
            .expect("hardcoded valid instant");
        let session = Session {
            id,
            workout_id: None,
            started_at: started,
            ended_at: Some(started + chrono::Duration::minutes(mins)),
            data_points: Vec::new(),
            rpe: None,
            ftp_watts: None,
            title: title.map(str::to_string),
            icu_id: None,
        };
        CalendarEvent::Session(
            SessionRecord {
                session,
                workout_name: None,
                uploaded_to_icu: false,
            },
            None,
        )
    }

    fn activity(
        icu_id: &str,
        name: &str,
        sport: &str,
        mins: u32,
        tss: Option<f32>,
    ) -> CalendarEvent {
        CalendarEvent::IcuActivity(IntervalsActivity {
            icu_id: icu_id.into(),
            date: day(),
            name: name.into(),
            tss,
            duration_secs: Some(mins * 60),
            average_watts: None,
            normalized_watts: None,
            average_hr: None,
            max_hr: None,
            sport_type: sport.into(),
            start_datetime_local: None,
            distance_m: None,
            elevation_gain_m: None,
            average_cadence: None,
        })
    }

    #[test]
    fn should_list_an_outdoor_ride_far_longer_than_the_planned_session() {
        // The live case this feature exists for: a 30-minute Active Recovery on
        // the plan, an 89-minute road ride actually done. Nearly three times the
        // planned length, and still obviously the day's training.
        let events = vec![activity(
            "i1",
            "Maltepe Road Cycling",
            "Ride",
            89,
            Some(95.0),
        )];
        let found = rides_on(day(), &events, FTP);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "Maltepe Road Cycling");
    }

    #[test]
    fn should_list_a_ride_much_shorter_than_the_planned_session() {
        // The other end of the same argument: 18 minutes against a planned 60.
        // Whether that really closes the day is the rider's call, not a rule's.
        let events = vec![session(4, Some("VO₂Max Staircase"), 18)];
        let found = rides_on(day(), &events, FTP);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "VO₂Max Staircase");
    }

    #[test]
    fn should_name_the_biggest_ride_of_the_day_first() {
        // 20 June holds both: a 21-minute spin and an 88-minute road ride. The
        // one-tap offer has to name the right one without the rider choosing.
        let events = vec![
            activity("i-short", "Kadıköy Road Cycling", "Ride", 21, Some(21.0)),
            activity("i-long", "Maltepe Road Cycling", "Ride", 88, Some(106.0)),
        ];
        let found = rides_on(day(), &events, FTP);
        assert_eq!(
            found.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["Maltepe Road Cycling", "Kadıköy Road Cycling"],
            "both are listed, hardest first"
        );
    }

    #[test]
    fn should_fall_back_to_length_when_neither_ride_carries_a_load_figure() {
        let events = vec![
            activity("i-short", "Short", "Ride", 30, None),
            activity("i-long", "Long", "Ride", 90, None),
        ];
        let found = rides_on(day(), &events, FTP);
        assert_eq!(
            found.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["Long", "Short"]
        );
    }

    #[test]
    fn should_ignore_activities_that_are_not_cycling() {
        // The rider swims, runs and does HIIT classes. None of them close a
        // planned bike session — `is_cycling` files HIIT as cross training.
        let events = vec![
            activity("i-run", "Morning Run", "Run", 45, Some(50.0)),
            activity(
                "i-hiit",
                "HIIT",
                "HighIntensityIntervalTraining",
                53,
                Some(27.0),
            ),
            activity("i-swim", "Swim", "Swim", 40, Some(30.0)),
        ];
        assert!(rides_on(day(), &events, FTP).is_empty());
    }

    #[test]
    fn should_recognise_cycling_under_any_of_its_names() {
        let events = vec![
            activity("i-v", "Zwift", "VirtualRide", 45, Some(50.0)),
            activity("i-g", "Gravel", "GravelRide", 90, Some(90.0)),
            activity("i-m", "MTB", "MountainBikeRide", 60, Some(70.0)),
        ];
        assert_eq!(rides_on(day(), &events, FTP).len(), 3);
    }

    #[test]
    fn should_ignore_a_ride_too_short_to_be_training() {
        // A trainer left recording, not a ride.
        let events = vec![activity("i1", "Stray", "Ride", 4, Some(2.0))];
        assert!(rides_on(day(), &events, FTP).is_empty());
    }

    #[test]
    fn should_count_a_ride_exactly_at_the_floor() {
        let events = vec![activity("i1", "Ten Minutes", "Ride", 10, Some(8.0))];
        assert_eq!(rides_on(day(), &events, FTP).len(), 1);
    }

    #[test]
    fn should_ignore_everything_on_another_day() {
        let events = vec![activity(
            "i1",
            "Maltepe Road Cycling",
            "Ride",
            89,
            Some(95.0),
        )];
        let other = NaiveDate::from_ymd_opt(2026, 8, 7).expect("hardcoded valid date");
        assert!(rides_on(other, &events, FTP).is_empty());
    }

    #[test]
    fn should_not_treat_the_plan_or_a_day_off_as_a_ride() {
        // A planned entry is the thing being closed, not a thing that closes it.
        let events = vec![CalendarEvent::TimeOff(TimeOffEntry {
            date: day(),
            notes: "Away".into(),
        })];
        assert!(rides_on(day(), &events, FTP).is_empty());
    }

    #[test]
    fn should_name_an_untitled_ride_rather_than_showing_a_blank() {
        // Garmin round-trips leave indoor rides with no title of their own.
        let events = vec![session(7, None, 55)];
        let found = rides_on(day(), &events, FTP);
        assert_eq!(found[0].name, "Recorded ride");
    }

    #[test]
    fn should_describe_a_ride_by_its_length_and_load() {
        let events = vec![activity(
            "i1",
            "Maltepe Road Cycling",
            "Ride",
            88,
            Some(106.0),
        )];
        assert_eq!(
            rides_on(day(), &events, FTP)[0].summary(),
            "88 min · 106 TSS"
        );
    }

    #[test]
    fn should_describe_a_ride_with_no_load_figure_by_its_length_alone() {
        let events = vec![activity("i1", "Maltepe Road Cycling", "Ride", 88, None)];
        assert_eq!(rides_on(day(), &events, FTP)[0].summary(), "88 min");
    }

    // ── Which days count as trained ─────────────────────────────────────────
    //
    // `trained_days` had no tests of its own: it decides whether the program
    // reads a day as ridden or as missed, and a missed day eases the week that
    // follows it.

    fn summary_of(secs: u64) -> crate::data::db::SessionSummary {
        crate::data::db::SessionSummary {
            id: 1,
            started_at: Utc
                .with_ymd_and_hms(2026, 8, 6, 7, 0, 0)
                .single()
                .expect("hardcoded valid instant"),
            duration_secs: secs,
            normalised_power: None,
            average_power: None,
            kilojoules: 0.0,
            ftp_watts: None,
            rpe: None,
            workout_name: None,
            uploaded_to_icu: false,
            icu_id: None,
        }
    }

    fn icu_of(sport: &str, secs: u32) -> IntervalsActivity {
        IntervalsActivity {
            icu_id: "icu-1".into(),
            date: day(),
            name: "Activity".into(),
            tss: None,
            duration_secs: Some(secs),
            average_watts: None,
            normalized_watts: None,
            average_hr: None,
            max_hr: None,
            sport_type: sport.into(),
            start_datetime_local: None,
            distance_m: None,
            elevation_gain_m: None,
            average_cadence: None,
        }
    }

    #[test]
    fn should_count_a_ride_of_exactly_the_countable_minimum() {
        let days = trained_days(&[summary_of(MIN_COUNTABLE_SECS as u64)], &[]);
        assert!(
            days.contains(&day()),
            "ten minutes is the minimum, not past it"
        );
    }

    #[test]
    fn should_not_count_a_ride_a_second_under_the_minimum() {
        let days = trained_days(&[summary_of(MIN_COUNTABLE_SECS as u64 - 1)], &[]);
        assert!(days.is_empty());
    }

    #[test]
    fn should_count_an_intervals_ride_of_exactly_the_minimum() {
        let days = trained_days(&[], &[icu_of("Ride", MIN_COUNTABLE_SECS)]);
        assert!(days.contains(&day()));
    }

    #[test]
    fn should_not_count_a_run_however_long_it_was() {
        // Both conditions have to hold: this is a training day for a runner,
        // and not one for the cycling program that reads it.
        let days = trained_days(&[], &[icu_of("Run", 7200)]);
        assert!(days.is_empty(), "a two-hour run is not a day on the bike");
    }

    #[test]
    fn should_not_count_a_short_ride_from_intervals() {
        let days = trained_days(&[], &[icu_of("Ride", MIN_COUNTABLE_SECS - 1)]);
        assert!(days.is_empty());
    }
}
