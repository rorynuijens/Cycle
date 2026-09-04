//! What the training program has to say about one calendar entry.
//!
//! This is the seam between the program's rules and the calendar's widgets: it
//! answers "does this entry belong to the plan, has it already been eased, and
//! is there an adjustment waiting for it" without touching GTK, so the answers
//! can be tested directly.
//!
//! It decides nothing on its own. The adjustment is produced by
//! [`crate::training::program::plan_view`] and merely routed to the entry it
//! names — the program stays the single authority.

use crate::data::db::CalendarEntry;
use crate::training::program::{week_of, Adjustment, Program};

/// An easing the program is proposing for this entry, ready to apply.
#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    pub to_workout_id: i64,
    pub to_name: String,
    /// The rider-facing sentence explaining why — [`crate::training::program::Reason::text`].
    pub reason: String,
}

/// Everything the calendar can say about an entry's place in the program.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EntryMark {
    /// Which week of the program this entry falls in, and how many there are.
    pub program_week: Option<(u32, u32)>,
    /// The workout the program originally asked for, when this has been eased.
    pub adjusted_from: Option<String>,
    /// The workout an Undo puts back — the rung below, not the origin.
    ///
    /// Undo steps back one ease at a time, so on a session eased twice this
    /// names the middle workout while `adjusted_from` still names the first.
    pub previous_step_name: Option<String>,
    /// An easing waiting to be applied.
    pub suggestion: Option<Suggestion>,
}

impl EntryMark {
    /// "Part of your program · week 2 of 4"
    pub fn program_text(&self) -> Option<String> {
        self.program_week
            .map(|(week, total)| format!("Part of your program · week {week} of {total}"))
    }

    /// The workout one Undo puts back — the rung below, or the origin when
    /// there is no chain (one step back from a single ease is home).
    pub fn undo_target(&self) -> Option<&str> {
        self.previous_step_name
            .as_deref()
            .or(self.adjusted_from.as_deref())
    }

    /// The heading for the "already eased" row.
    ///
    /// Naming the destination is what makes a chain of eases legible: a session
    /// eased twice goes back somewhere that is *not* what the subtitle calls the
    /// origin, and a bare "Undo" cannot say which. It lives in the row rather
    /// than on the button because a button carrying a workout name truncates to
    /// "Undo — back to R…" at every width the app actually has.
    ///
    /// While the two coincide — a single ease, where one press goes home — the
    /// row says nothing extra, rather than printing the same workout name twice.
    pub fn undo_title(&self) -> String {
        match self.undo_target() {
            Some(target) if Some(target) != self.adjusted_from.as_deref() => {
                format!("Undo goes back to {target}")
            }
            _ => "Eased by your program".to_string(),
        }
    }
}

/// The program's state for one calendar render.
///
/// Built once per reload and shared by both views, so the month grid and the
/// week list cannot disagree about which day the plan wants eased.
#[derive(Debug, Clone, Default)]
pub struct ProgramOverlay {
    pub program: Option<Program>,
    /// The single easing the program is proposing, if any. `suggest` returns at
    /// most one, for the next session only.
    pub adjustment: Option<Adjustment>,
}

impl ProgramOverlay {
    /// What to show for one entry.
    pub fn mark(&self, entry: &CalendarEntry) -> EntryMark {
        mark_for(entry, self.program.as_ref(), self.adjustment.as_ref())
    }
}

/// Read one entry against the active program and its pending adjustment.
///
/// A completed entry carries its program line but neither action: the ride has
/// happened, and `apply_adjustment` would refuse it anyway.
pub fn mark_for(
    entry: &CalendarEntry,
    program: Option<&Program>,
    adjustment: Option<&Adjustment>,
) -> EntryMark {
    // Membership is read off the row, not inferred from the date — a hand-added
    // entry sitting on a program day is still not part of the program.
    let program_week = program
        .filter(|p| entry.program_id == Some(p.id))
        .and_then(|p| {
            let date = entry.date()?;
            Some((week_of(p, date), p.num_weeks))
        });

    if entry.completed {
        return EntryMark {
            program_week,
            ..EntryMark::default()
        };
    }

    let suggestion = adjustment
        .filter(|a| a.entry_id == entry.id)
        .map(|a| Suggestion {
            to_workout_id: a.to_workout_id,
            to_name: a.to_name.clone(),
            reason: a.reason.text(),
        });

    EntryMark {
        program_week,
        adjusted_from: entry.adjusted_from.clone(),
        previous_step_name: entry.previous_step_name.clone(),
        suggestion,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::db::ScheduledItem;
    use crate::data::workout::WorkoutCategory;
    use crate::training::program::Reason;
    use chrono::NaiveDate;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("valid test date")
    }

    fn program() -> Program {
        Program {
            id: 7,
            start_monday: date(2026, 8, 3),
            num_weeks: 8,
            training_days: "Mon,Wed,Fri".into(),
        }
    }

    fn entry(id: i64, day: &str, program_id: Option<i64>) -> CalendarEntry {
        CalendarEntry {
            id,
            item: ScheduledItem::Workout {
                id: 100,
                name: "Threshold 2x20".into(),
            },
            scheduled_date: day.into(),
            completed: false,
            category: WorkoutCategory::Threshold,
            tss: 85.0,
            duration_secs: 3600,
            program_id,
            adjusted_from: None,
            previous_step_name: None,
        }
    }

    fn route_entry(id: i64, day: &str) -> CalendarEntry {
        CalendarEntry {
            id,
            item: ScheduledItem::Route {
                id: 3,
                name: "Alpe".into(),
            },
            scheduled_date: day.into(),
            completed: false,
            category: WorkoutCategory::Endurance,
            tss: 90.0,
            duration_secs: 5400,
            program_id: None,
            adjusted_from: None,
            previous_step_name: None,
        }
    }

    fn adjustment(entry_id: i64) -> Adjustment {
        Adjustment {
            entry_id,
            date: date(2026, 8, 12),
            from_workout_id: 100,
            from_name: "Threshold 2x20".into(),
            to_workout_id: 200,
            to_name: "Sweet Spot 3x12".into(),
            reason: Reason::Fatigued { tsb: -18.0 },
        }
    }

    #[test]
    fn should_report_program_week_when_entry_belongs_to_the_program() {
        // 2026-08-12 is the Wednesday of the program's second week.
        let mark = mark_for(&entry(1, "2026-08-12", Some(7)), Some(&program()), None);
        assert_eq!(mark.program_week, Some((2, 8)));
        assert_eq!(
            mark.program_text().as_deref(),
            Some("Part of your program · week 2 of 8")
        );
    }

    #[test]
    fn should_not_mark_a_hand_scheduled_entry_on_a_program_day() {
        let mark = mark_for(&entry(1, "2026-08-12", None), Some(&program()), None);
        assert_eq!(mark.program_week, None);
        assert_eq!(mark, EntryMark::default());
    }

    #[test]
    fn should_not_mark_an_entry_owned_by_a_different_program() {
        let mark = mark_for(&entry(1, "2026-08-12", Some(99)), Some(&program()), None);
        assert_eq!(mark.program_week, None);
    }

    #[test]
    fn should_never_mark_a_route() {
        // Routes carry no program id, so they fall out without a special case.
        let mark = mark_for(&route_entry(5, "2026-08-12"), Some(&program()), None);
        assert_eq!(mark, EntryMark::default());
    }

    #[test]
    fn should_attach_the_suggestion_only_to_the_entry_it_names() {
        let adj = adjustment(1);
        let p = program();

        let target = mark_for(&entry(1, "2026-08-12", Some(7)), Some(&p), Some(&adj));
        assert_eq!(
            target.suggestion.as_ref().map(|s| s.to_name.as_str()),
            Some("Sweet Spot 3x12")
        );
        assert_eq!(
            target.suggestion.as_ref().map(|s| s.to_workout_id),
            Some(200)
        );

        let other = mark_for(&entry(2, "2026-08-14", Some(7)), Some(&p), Some(&adj));
        assert_eq!(other.suggestion, None);
        // Still in the program, though — the two facts are independent.
        assert_eq!(other.program_week, Some((2, 8)));
    }

    #[test]
    fn should_carry_the_reason_sentence_with_the_suggestion() {
        let adj = adjustment(1);
        let mark = mark_for(
            &entry(1, "2026-08-12", Some(7)),
            Some(&program()),
            Some(&adj),
        );
        let reason = mark.suggestion.expect("suggestion present").reason;
        assert!(reason.contains("-18"), "got {reason:?}");
    }

    #[test]
    fn should_offer_no_action_on_a_completed_entry() {
        let mut e = entry(1, "2026-08-12", Some(7));
        e.completed = true;
        e.adjusted_from = Some("Threshold 2x20".into());

        let mark = mark_for(&e, Some(&program()), Some(&adjustment(1)));
        // The ride happened: neither applying nor undoing is on offer.
        assert_eq!(mark.suggestion, None);
        assert_eq!(mark.adjusted_from, None);
        // But it is still visibly part of the plan.
        assert_eq!(mark.program_week, Some((2, 8)));
    }

    #[test]
    fn should_report_an_already_eased_entry() {
        let mut e = entry(1, "2026-08-12", Some(7));
        e.adjusted_from = Some("Threshold 2x20".into());
        let mark = mark_for(&e, Some(&program()), None);
        assert_eq!(mark.adjusted_from.as_deref(), Some("Threshold 2x20"));
        assert_ne!(mark, EntryMark::default());
    }

    #[test]
    fn should_carry_the_step_below_as_well_as_the_origin() {
        let mut e = entry(1, "2026-08-12", Some(7));
        e.adjusted_from = Some("Threshold 2x20".into());
        e.previous_step_name = Some("Sweet Spot 3x12".into());
        let mark = mark_for(&e, Some(&program()), None);
        assert_eq!(mark.previous_step_name.as_deref(), Some("Sweet Spot 3x12"));
    }

    #[test]
    fn should_name_the_step_the_undo_lands_on() {
        let mark = EntryMark {
            adjusted_from: Some("Threshold 2x20".into()),
            previous_step_name: Some("Sweet Spot 3x12".into()),
            ..EntryMark::default()
        };
        assert_eq!(mark.undo_target(), Some("Sweet Spot 3x12"));
        assert_eq!(mark.undo_title(), "Undo goes back to Sweet Spot 3x12");
    }

    #[test]
    fn should_stay_quiet_when_one_press_goes_all_the_way_home() {
        // Eased once: the step below and the origin are the same workout, and
        // naming it in both the title and the subtitle says it twice.
        let mark = EntryMark {
            adjusted_from: Some("Threshold 2x20".into()),
            previous_step_name: Some("Threshold 2x20".into()),
            ..EntryMark::default()
        };
        assert_eq!(mark.undo_title(), "Eased by your program");
    }

    #[test]
    fn should_fall_back_to_the_origin_when_there_is_no_chain() {
        // A database eased before v5 knows only where the plan started, and one
        // step back from a single ease is the origin anyway.
        let mark = EntryMark {
            adjusted_from: Some("Threshold 2x20".into()),
            ..EntryMark::default()
        };
        assert_eq!(mark.undo_target(), Some("Threshold 2x20"));
        assert_eq!(mark.undo_title(), "Eased by your program");
    }

    #[test]
    fn should_name_nothing_when_it_can_name_nothing() {
        assert_eq!(EntryMark::default().undo_target(), None);
        assert_eq!(EntryMark::default().undo_title(), "Eased by your program");
    }

    #[test]
    fn should_offer_no_undo_target_on_a_completed_entry() {
        // `mark_for` short-circuits on a completed entry, and the button that
        // reads this is hidden there — but a label naming a workout the day was
        // never put back to would be wrong if it ever were not.
        let mut e = entry(1, "2026-08-12", Some(7));
        e.completed = true;
        e.adjusted_from = Some("Threshold 2x20".into());
        e.previous_step_name = Some("Sweet Spot 3x12".into());

        let mark = mark_for(&e, Some(&program()), None);
        assert_eq!(mark.previous_step_name, None);
        assert_eq!(mark.undo_target(), None);
    }

    #[test]
    fn should_be_blank_when_no_program_is_active() {
        let mark = mark_for(&entry(1, "2026-08-12", Some(7)), None, None);
        assert_eq!(mark, EntryMark::default());
    }

    #[test]
    fn should_not_mark_an_entry_whose_date_is_unreadable() {
        let mark = mark_for(&entry(1, "not-a-date", Some(7)), Some(&program()), None);
        assert_eq!(mark.program_week, None);
    }
}
