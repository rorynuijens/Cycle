//! What sits on a day of the training calendar, and what it is worth.
//!
//! The calendar merges four sources — workouts scheduled ahead, rides this app
//! recorded, activities synced from Intervals.icu, and days marked off — into
//! one timeline. The arithmetic over that timeline used to live in the page
//! that draws it; it is plain data here, with no GTK (CLAUDE.md §2.6).

use std::collections::HashMap;

use chrono::{Datelike, NaiveDate};

use crate::data::db::{CalendarEntry, IntervalsActivity, SessionRecord, TimeOffEntry};

/// One thing shown on a calendar day, from whichever source it came.
#[derive(Clone)]
pub enum CalendarEvent {
    /// A workout planned for a date, possibly already completed.
    Scheduled(CalendarEntry),
    /// A ride this app recorded, with the name to file it under.
    Session(SessionRecord, Option<String>),
    /// An activity synced from Intervals.icu.
    IcuActivity(IntervalsActivity),
    /// A day the rider marked as off.
    TimeOff(TimeOffEntry),
}

impl CalendarEvent {
    /// The local date this event belongs to.
    ///
    /// `None` only for a scheduled entry whose stored date will not parse —
    /// the calendar table holds a text date, so a corrupt or hand-edited row
    /// can reach here (CLAUDE.md §5.2). Callers skip those rather than guess.
    pub fn date(&self) -> Option<NaiveDate> {
        match self {
            Self::Scheduled(e) => NaiveDate::parse_from_str(&e.scheduled_date, "%Y-%m-%d").ok(),
            Self::Session(s, _) => Some(
                s.session
                    .started_at
                    .with_timezone(&chrono::Local)
                    .date_naive(),
            ),
            Self::IcuActivity(a) => Some(a.date),
            Self::TimeOff(t) => Some(t.date),
        }
    }

    /// What this event contributes to a day's training load.
    ///
    /// `fallback_ftp` scores rides recorded before FTP was stamped on them;
    /// a stamped ride is scored against the FTP it was actually ridden at.
    pub fn load(&self, fallback_ftp: u32) -> EventLoad {
        match self {
            Self::Scheduled(e) => EventLoad {
                // A completed plan has been banked; an open one is still ahead.
                done_tss: if e.completed { e.tss } else { 0.0 },
                planned_tss: if e.completed { 0.0 } else { e.tss },
                is_scheduled: true,
                is_complete: e.completed,
            },
            Self::Session(s, _) => EventLoad {
                done_tss: s.session.tss(fallback_ftp).unwrap_or(0.0),
                planned_tss: 0.0,
                is_scheduled: false,
                is_complete: true,
            },
            Self::IcuActivity(a) => EventLoad {
                done_tss: a.tss.unwrap_or(0.0),
                planned_tss: 0.0,
                is_scheduled: false,
                is_complete: true,
            },
            Self::TimeOff(_) => EventLoad::default(),
        }
    }
}

/// One event's contribution to a day's load.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct EventLoad {
    /// Training stress already banked.
    pub done_tss: f32,
    /// Training stress still ahead.
    pub planned_tss: f32,
    /// Whether this came from the plan rather than from something ridden.
    pub is_scheduled: bool,
    pub is_complete: bool,
}

/// Load summed over a set of events — a day, a week, or a month.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LoadTotals {
    pub done_tss: f32,
    pub planned_tss: f32,
    /// Scheduled workouts in the range.
    pub scheduled: usize,
    /// How many of those are done.
    pub scheduled_done: usize,
}

impl LoadTotals {
    /// Everything on the books, ridden or not.
    pub fn total_tss(&self) -> f32 {
        self.done_tss + self.planned_tss
    }

    /// True when the range holds nothing at all.
    pub fn is_empty(&self) -> bool {
        self.total_tss() == 0.0 && self.scheduled == 0
    }
}

/// Sum the load across `events`.
pub fn totals<'a>(
    events: impl IntoIterator<Item = &'a CalendarEvent>,
    fallback_ftp: u32,
) -> LoadTotals {
    let mut t = LoadTotals::default();
    for event in events {
        let load = event.load(fallback_ftp);
        t.done_tss += load.done_tss;
        t.planned_tss += load.planned_tss;
        if load.is_scheduled {
            t.scheduled += 1;
            if load.is_complete {
                t.scheduled_done += 1;
            }
        }
    }
    t
}

/// Bucket events by the day they fall on. Events with an unparseable date are
/// dropped rather than landing on an arbitrary day.
pub fn group_by_date(events: &[CalendarEvent]) -> HashMap<NaiveDate, Vec<&CalendarEvent>> {
    let mut by_day: HashMap<NaiveDate, Vec<&CalendarEvent>> = HashMap::new();
    for event in events {
        if let Some(date) = event.date() {
            by_day.entry(date).or_default().push(event);
        }
    }
    by_day
}

/// Number of days in a calendar month. Returns 0 for a month outside the
/// representable range rather than panicking.
pub fn days_in_month(year: i32, month: u32) -> u32 {
    let first = match NaiveDate::from_ymd_opt(year, month, 1) {
        Some(d) => d,
        None => return 0,
    };
    let next = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)
    };
    match next {
        Some(n) => n.signed_duration_since(first).num_days() as u32,
        None => 0,
    }
}

/// The first and last dates of a calendar month.
pub fn month_bounds(year: i32, month: u32) -> Option<(NaiveDate, NaiveDate)> {
    let first = NaiveDate::from_ymd_opt(year, month, 1)?;
    let last = NaiveDate::from_ymd_opt(year, month, days_in_month(year, month))?;
    Some((first, last))
}

/// Heading for a month view, e.g. "August 2026".
pub fn month_label(year: i32, month: u32) -> String {
    NaiveDate::from_ymd_opt(year, month, 1)
        .map(|d| d.format("%B %Y").to_string())
        .unwrap_or_default()
}

/// Which grid column (0 = Monday) the first of the month falls in.
pub fn first_weekday_column(year: i32, month: u32) -> u32 {
    NaiveDate::from_ymd_opt(year, month, 1)
        .map(|d| d.weekday().num_days_from_monday())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::db::SessionRecord;
    use crate::data::session::{DataPoint, Session};
    use crate::data::workout::WorkoutCategory;
    use chrono::{TimeZone, Utc};

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("valid test date")
    }

    fn scheduled(day: &str, tss: f32, completed: bool) -> CalendarEvent {
        CalendarEvent::Scheduled(CalendarEntry {
            id: 1,
            workout_id: 1,
            workout_name: "Endurance 60".into(),
            scheduled_date: day.into(),
            completed,
            category: WorkoutCategory::Endurance,
            tss,
            duration_secs: 3600,
        })
    }

    fn icu(day: NaiveDate, tss: Option<f32>) -> CalendarEvent {
        CalendarEvent::IcuActivity(IntervalsActivity {
            icu_id: "i1".into(),
            date: day,
            name: "Ride".into(),
            tss,
            duration_secs: Some(3600),
            average_watts: Some(200),
            normalized_watts: None,
            average_hr: None,
            max_hr: None,
            sport_type: "Ride".into(),
            start_datetime_local: None,
            distance_m: None,
            elevation_gain_m: None,
            average_cadence: None,
        })
    }

    /// A one-hour session at `watts`, ridden at noon local on `day`.
    fn session(day: NaiveDate, watts: u32) -> CalendarEvent {
        let mut s = Session::new(None);
        s.started_at = Utc.from_utc_datetime(&day.and_hms_opt(12, 0, 0).expect("valid time"));
        s.data_points = (0..3600)
            .map(|i| DataPoint {
                elapsed_secs: i,
                power_watts: Some(watts),
                target_watts: None,
                heart_rate_bpm: None,
                cadence_rpm: None,
                speed_kmh: None,
                altitude_m: None,
                lat: None,
                lng: None,
            })
            .collect();
        s.ended_at = Some(s.started_at + chrono::Duration::seconds(3600));
        CalendarEvent::Session(
            SessionRecord {
                session: s,
                workout_name: None,
                uploaded_to_icu: false,
            },
            None,
        )
    }

    // ── dates ────────────────────────────────────────────────────────────────

    #[test]
    fn should_read_the_date_off_each_kind_of_event() {
        assert_eq!(
            scheduled("2026-08-05", 60.0, false).date(),
            Some(date(2026, 8, 5))
        );
        assert_eq!(icu(date(2026, 8, 6), None).date(), Some(date(2026, 8, 6)));
        assert_eq!(
            session(date(2026, 8, 7), 200).date(),
            Some(date(2026, 8, 7))
        );
        assert_eq!(
            CalendarEvent::TimeOff(TimeOffEntry {
                date: date(2026, 8, 8),
                notes: String::new(),
            })
            .date(),
            Some(date(2026, 8, 8))
        );
    }

    #[test]
    fn should_refuse_to_place_an_entry_with_an_unparseable_date() {
        // The calendar table stores a text date, so a corrupt row can reach here.
        assert_eq!(scheduled("not-a-date", 60.0, false).date(), None);
        assert_eq!(scheduled("", 60.0, false).date(), None);
    }

    #[test]
    fn should_drop_undateable_events_when_grouping() {
        let events = vec![
            scheduled("2026-08-05", 60.0, false),
            scheduled("junk", 60.0, false),
        ];
        let by_day = group_by_date(&events);
        assert_eq!(by_day.len(), 1);
        assert_eq!(by_day[&date(2026, 8, 5)].len(), 1);
    }

    #[test]
    fn should_group_several_events_onto_one_day() {
        let day = date(2026, 8, 5);
        let events = vec![
            scheduled("2026-08-05", 60.0, false),
            icu(day, Some(40.0)),
            session(day, 200),
        ];
        assert_eq!(group_by_date(&events)[&day].len(), 3);
    }

    // ── load ─────────────────────────────────────────────────────────────────

    #[test]
    fn should_count_an_open_plan_as_still_ahead() {
        let load = scheduled("2026-08-05", 60.0, false).load(250);
        assert_eq!(load.planned_tss, 60.0);
        assert_eq!(load.done_tss, 0.0);
        assert!(load.is_scheduled);
        assert!(!load.is_complete);
    }

    #[test]
    fn should_count_a_completed_plan_as_banked() {
        let load = scheduled("2026-08-05", 60.0, true).load(250);
        assert_eq!(load.done_tss, 60.0);
        assert_eq!(
            load.planned_tss, 0.0,
            "a done workout is not also still ahead"
        );
        assert!(load.is_complete);
    }

    #[test]
    fn should_count_a_synced_activity_as_banked_and_unscheduled() {
        let load = icu(date(2026, 8, 5), Some(42.0)).load(250);
        assert_eq!(load.done_tss, 42.0);
        assert!(!load.is_scheduled, "a synced ride was never on the plan");
        assert!(load.is_complete);
    }

    #[test]
    fn should_contribute_no_load_for_an_activity_with_no_tss() {
        assert_eq!(icu(date(2026, 8, 5), None).load(250).done_tss, 0.0);
    }

    #[test]
    fn should_contribute_no_load_for_a_day_off() {
        let load = CalendarEvent::TimeOff(TimeOffEntry {
            date: date(2026, 8, 5),
            notes: "holiday".into(),
        })
        .load(250);
        assert_eq!(load, EventLoad::default());
        assert!(!load.is_scheduled);
    }

    #[test]
    fn should_score_a_ride_against_the_ftp_it_was_ridden_at() {
        let day = date(2026, 8, 5);
        let CalendarEvent::Session(mut record, name) = session(day, 250) else {
            panic!("expected a session");
        };
        record.session.ftp_watts = Some(250);
        let stamped = CalendarEvent::Session(record, name);
        // An hour at a stamped FTP of 250 is ~100 TSS regardless of the
        // fallback, so raising the profile FTP must not deflate past work.
        let at_stamped = stamped.load(250).done_tss;
        let with_higher_fallback = stamped.load(400).done_tss;
        assert!((at_stamped - with_higher_fallback).abs() < 0.01);
        assert!(at_stamped > 95.0 && at_stamped < 105.0, "{at_stamped}");
    }

    // ── totals ───────────────────────────────────────────────────────────────

    #[test]
    fn should_total_nothing_for_no_events() {
        let t = totals(&[], 250);
        assert_eq!(t, LoadTotals::default());
        assert!(t.is_empty());
    }

    #[test]
    fn should_split_totals_between_banked_and_still_ahead() {
        let events = vec![
            scheduled("2026-08-03", 60.0, true),
            scheduled("2026-08-05", 80.0, false),
            icu(date(2026, 8, 4), Some(40.0)),
        ];
        let t = totals(&events, 250);
        assert_eq!(t.done_tss, 100.0, "60 completed plan + 40 synced");
        assert_eq!(t.planned_tss, 80.0);
        assert_eq!(t.total_tss(), 180.0);
        assert!(!t.is_empty());
    }

    #[test]
    fn should_count_only_scheduled_workouts_in_the_done_ratio() {
        // "2/3 done" must not be inflated by rides that were never planned.
        let events = vec![
            scheduled("2026-08-03", 60.0, true),
            scheduled("2026-08-04", 60.0, true),
            scheduled("2026-08-05", 60.0, false),
            icu(date(2026, 8, 6), Some(40.0)),
        ];
        let t = totals(&events, 250);
        assert_eq!(t.scheduled, 3);
        assert_eq!(t.scheduled_done, 2);
    }

    #[test]
    fn should_not_count_a_day_off_as_a_scheduled_workout() {
        let events = vec![CalendarEvent::TimeOff(TimeOffEntry {
            date: date(2026, 8, 5),
            notes: String::new(),
        })];
        let t = totals(&events, 250);
        assert_eq!(t.scheduled, 0);
        assert!(t.is_empty());
    }

    // ── month arithmetic ─────────────────────────────────────────────────────

    #[test]
    fn should_return_the_length_of_each_month() {
        assert_eq!(days_in_month(2026, 1), 31);
        assert_eq!(days_in_month(2026, 4), 30);
        assert_eq!(days_in_month(2026, 2), 28);
        assert_eq!(days_in_month(2026, 12), 31, "December must roll the year");
    }

    #[test]
    fn should_return_29_days_in_a_leap_february() {
        assert_eq!(days_in_month(2028, 2), 29);
        assert_eq!(days_in_month(2000, 2), 29, "a 400-year leap year");
        assert_eq!(
            days_in_month(1900, 2),
            28,
            "a century that is not a leap year"
        );
    }

    #[test]
    fn should_return_zero_days_for_an_invalid_month() {
        // Previously this path unwrapped and would have panicked.
        assert_eq!(days_in_month(2026, 0), 0);
        assert_eq!(days_in_month(2026, 13), 0);
    }

    #[test]
    fn should_return_the_first_and_last_day_of_a_month() {
        assert_eq!(
            month_bounds(2026, 2),
            Some((date(2026, 2, 1), date(2026, 2, 28)))
        );
        assert_eq!(
            month_bounds(2026, 12),
            Some((date(2026, 12, 1), date(2026, 12, 31)))
        );
        assert_eq!(month_bounds(2026, 13), None);
    }

    #[test]
    fn should_label_a_month_with_its_name_and_year() {
        assert_eq!(month_label(2026, 8), "August 2026");
        assert_eq!(month_label(2026, 13), "");
    }

    #[test]
    fn should_put_the_first_of_the_month_in_the_right_column() {
        // 1 August 2026 is a Saturday — column 5 in a Monday-first grid.
        assert_eq!(first_weekday_column(2026, 8), 5);
        // 1 June 2026 is a Monday.
        assert_eq!(first_weekday_column(2026, 6), 0);
    }
}
