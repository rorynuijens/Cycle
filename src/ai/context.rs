//! Turning stored training data into the shapes the prompts are built from,
//! and reading the model's reply back out.
//!
//! These conversions used to live in the Coaching and Fitness pages. They are
//! plain data in, plain data out — no GTK (CLAUDE.md §2.6) — so the rules about
//! what the coach is told (and what it is not) can be tested directly.

use chrono::Local;

use crate::ai::briefing::PlannedWorkout;
use crate::ai::coach::{ProgramEntry, RecentSession, WellnessSnapshot, WorkoutOption};
use crate::data::db::{IntervalsActivity, IntervalsWorkout, SessionSummary, WellnessEntry};
use crate::data::sport::{is_cycling, normalize_sport_type};
use crate::data::workout::Workout;

/// Describe one of the app's own rides to the coach.
pub fn build_recent_session(r: &SessionSummary, ftp: u32) -> RecentSession {
    RecentSession {
        date: r
            .started_at
            .with_timezone(&Local)
            .format("%Y-%m-%d")
            .to_string(),
        name: r.workout_name.clone(),
        // Sessions the app records are always indoor rides.
        sport_type: "Cycling".to_string(),
        duration_mins: (r.duration_secs / 60) as u32,
        avg_power: r.average_power.map(|w| w as u32),
        tss: r.tss(ftp),
        kj: r.kilojoules,
        rpe: r.rpe,
    }
}

/// Describe an activity synced from Intervals.icu to the coach.
///
/// Power and work are reported only for cycling: a running power meter (Stryd
/// and similar) measures something different, and feeding it to the coach as
/// though it were bike watts produces nonsense about the rider's form.
pub fn icu_activity_to_recent_session(a: &IntervalsActivity) -> RecentSession {
    let cycling = is_cycling(&a.sport_type);
    RecentSession {
        date: a.date.format("%Y-%m-%d").to_string(),
        // Fall back to the sport rather than nothing when the name is blank.
        name: if a.name.is_empty() {
            None
        } else {
            Some(a.name.clone())
        },
        sport_type: normalize_sport_type(&a.sport_type),
        duration_mins: a.duration_secs.map(|s| s / 60).unwrap_or(0),
        avg_power: if cycling { a.average_watts } else { None },
        tss: a.tss,
        kj: if cycling {
            a.average_watts
                .zip(a.duration_secs)
                .map(|(w, d)| w as f32 * d as f32 / 1000.0)
                .unwrap_or(0.0)
        } else {
            0.0
        },
        rpe: None,
    }
}

/// Convert stored wellness rows into the snapshot shape the prompts take.
pub fn wellness_snapshots(entries: &[WellnessEntry]) -> Vec<WellnessSnapshot> {
    entries
        .iter()
        .map(|w| WellnessSnapshot {
            date: w.date.format("%Y-%m-%d").to_string(),
            hrv: w.hrv,
            resting_hr: w.resting_hr,
            sleep_hours: w.sleep_secs.map(|s| s as f32 / 3600.0),
            sleep_score: w.sleep_score,
            steps: w.steps,
            calories: w.calories,
        })
        .collect()
}

/// The prefix marking an Intervals.icu template in a workout menu, so the coach
/// can name one and the page can find it again.
pub const ICU_PREFIX: &str = "[Intervals.icu] ";

/// The menu of workouts the coach is allowed to choose from.
pub fn workouts_as_options(
    workouts: &[Workout],
    icu_workouts: &[IntervalsWorkout],
) -> Vec<WorkoutOption> {
    let mut opts: Vec<WorkoutOption> = workouts
        .iter()
        .map(|w| WorkoutOption {
            name: w.name.clone(),
            duration_mins: w.duration_secs / 60,
            tss: w.tss,
            category: w.category.label().to_string(),
        })
        .collect();

    for w in icu_workouts {
        opts.push(WorkoutOption {
            name: format!("{ICU_PREFIX}{}", w.name),
            // A template with no duration is still offerable; assume an hour so
            // the coach can weigh it against the rest of the menu.
            duration_mins: w.duration_secs.map(|s| s / 60).unwrap_or(60),
            tss: w.tss.unwrap_or(0.0),
            category: "Intervals.icu".to_string(),
        });
    }
    opts
}

/// Describe a workout to the coach as the one planned for today.
pub fn planned_from_workout(w: &Workout) -> PlannedWorkout {
    PlannedWorkout {
        name: w.name.clone(),
        duration_mins: w.duration_secs / 60,
        tss: w.tss,
        category: w.category.label().to_string(),
    }
}

/// Work out which workout today's briefing should be written around.
///
/// A workout actually on the calendar wins. With the day empty, the cached
/// coaching suggestion stands in, so the Morning Brief and the Coaching page do
/// not contradict each other. Returns `None` when the day is genuinely open —
/// or when the cached suggestion names a workout no longer in the library.
pub fn resolve_planned_workout(
    scheduled: Option<&Workout>,
    cached_suggestion_name: &str,
    library: &[Workout],
) -> Option<PlannedWorkout> {
    if let Some(w) = scheduled {
        return Some(planned_from_workout(w));
    }
    let wanted = cached_suggestion_name.trim();
    if wanted.is_empty() {
        return None;
    }
    library
        .iter()
        .find(|w| crate::ai::naming::names_match(&w.name, wanted))
        .map(planned_from_workout)
}

/// The workout named on the reply's `RECOMMENDED_WORKOUT:` marker line.
pub fn extract_recommended_workout(text: &str) -> Option<String> {
    crate::ai::naming::extract_marker_value(text, "RECOMMENDED_WORKOUT:")
}

/// The reply with its marker line removed, ready to show the rider.
pub fn strip_recommended_line(text: &str) -> String {
    text.lines()
        .filter(|l| !l.trim_start().starts_with("RECOMMENDED_WORKOUT:"))
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_string()
}

/// Render a parsed multi-week program as markdown, annotating each day with the
/// duration of the workout it names.
pub fn format_program(
    entries: &[ProgramEntry],
    workouts: &[Workout],
    icu_workouts: &[IntervalsWorkout],
) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let mut lines: Vec<String> = Vec::new();
    let mut current_week = 0u32;
    for entry in entries {
        if entry.week != current_week {
            current_week = entry.week;
            if !lines.is_empty() {
                lines.push(String::new());
            }
            lines.push(format!("## Week {}", current_week));
        }
        let duration_note = workouts
            .iter()
            .find(|w| crate::ai::naming::names_match(&w.name, &entry.workout_name))
            .map(|w| format!(" ({} min)", w.duration_secs / 60))
            .or_else(|| {
                let lookup = entry
                    .workout_name
                    .strip_prefix(ICU_PREFIX)
                    .unwrap_or(&entry.workout_name);
                icu_workouts
                    .iter()
                    .find(|w| crate::ai::naming::names_match(&w.name, lookup))
                    .and_then(|w| w.duration_secs)
                    .map(|s| format!(" ({} min) [Intervals.icu]", s / 60))
            })
            .unwrap_or_default();
        lines.push(format!(
            "- {} — {}{}",
            capitalize_first(&entry.day),
            entry.workout_name,
            duration_note
        ));
    }
    lines.join("\n")
}

/// Upper-case the first character, leaving the rest alone.
pub fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

/// Days after Monday for a weekday name. An unrecognised day falls back to
/// Monday rather than dropping the entry — a scheduled workout the rider can
/// move beats one that never appears.
pub fn day_name_to_offset(day: &str) -> u32 {
    match day.to_lowercase().as_str() {
        "tuesday" => 1,
        "wednesday" => 2,
        "thursday" => 3,
        "friday" => 4,
        "saturday" => 5,
        "sunday" => 6,
        _ => 0,
    }
}

/// Split a numbered analysis reply into `(heading, body)` sections.
///
/// The models reliably answer in a numbered list ("1. **Training Load**: …"),
/// which reads better as separate cards than as one wall of text. Returns an
/// empty vec when fewer than two sections are found, so the caller falls back
/// to showing the reply verbatim rather than mangling an unexpected shape.
pub fn parse_ai_sections(text: &str) -> Vec<(String, String)> {
    let mut sections: Vec<(String, String)> = Vec::new();
    let mut current_heading = String::new();
    let mut current_body: Vec<&str> = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim();
        // A heading looks like "1. **Training Load Summary**:" or "2. Trend".
        // The length cap allows for markdown decoration without swallowing a
        // body paragraph that happens to open with a number.
        let is_section_head = trimmed.len() < 100
            && trimmed.chars().next().is_some_and(|c| c.is_ascii_digit())
            && trimmed.contains(". ")
            && !trimmed.starts_with("0.");

        if is_section_head {
            if !current_heading.is_empty() {
                let body = current_body.join("\n").trim().to_string();
                if !body.is_empty() {
                    sections.push((current_heading.clone(), body));
                }
                current_body.clear();
            }
            current_heading = trimmed
                .split_once(". ")
                .map(|x| x.1)
                .unwrap_or(trimmed)
                .trim_end_matches(':')
                .to_string();
        } else if !current_heading.is_empty() {
            current_body.push(trimmed);
        }
    }
    if !current_heading.is_empty() {
        let body = current_body.join("\n").trim().to_string();
        if !body.is_empty() {
            sections.push((current_heading, body));
        }
    }

    if sections.len() < 2 {
        return Vec::new();
    }
    sections
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("valid test date")
    }

    fn icu(sport: &str, watts: Option<u32>, secs: Option<u32>) -> IntervalsActivity {
        IntervalsActivity {
            icu_id: "i1".into(),
            date: date(2026, 8, 5),
            name: "Morning session".into(),
            tss: Some(42.0),
            duration_secs: secs,
            average_watts: watts,
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

    // ── icu_activity_to_recent_session ───────────────────────────────────────

    #[test]
    fn should_report_power_and_work_for_a_ride() {
        let s = icu_activity_to_recent_session(&icu("Ride", Some(200), Some(3600)));
        assert_eq!(s.sport_type, "Cycling");
        assert_eq!(s.avg_power, Some(200));
        assert!((s.kj - 720.0).abs() < 0.01, "{}", s.kj);
        assert_eq!(s.duration_mins, 60);
    }

    #[test]
    fn should_withhold_power_from_a_run() {
        // Stryd running power is not bike watts — the coach must not read it as
        // though it were.
        let s = icu_activity_to_recent_session(&icu("Run", Some(280), Some(1800)));
        assert_eq!(s.sport_type, "Run");
        assert_eq!(s.avg_power, None);
        assert_eq!(s.kj, 0.0);
        assert_eq!(s.tss, Some(42.0), "TSS is still reported for a run");
    }

    #[test]
    fn should_report_power_for_a_gravel_ride() {
        // A gravel ride is cycling; its power belongs in the picture.
        let s = icu_activity_to_recent_session(&icu("GravelRide", Some(180), Some(7200)));
        assert_eq!(s.sport_type, "Cycling");
        assert_eq!(s.avg_power, Some(180));
        assert!(s.kj > 0.0);
    }

    #[test]
    fn should_fall_back_to_no_name_when_the_activity_is_unnamed() {
        let mut a = icu("Ride", Some(200), Some(3600));
        a.name = String::new();
        assert_eq!(icu_activity_to_recent_session(&a).name, None);
    }

    #[test]
    fn should_report_zero_duration_when_the_activity_has_none() {
        let s = icu_activity_to_recent_session(&icu("Ride", Some(200), None));
        assert_eq!(s.duration_mins, 0);
        assert_eq!(s.kj, 0.0, "no duration means no computable work");
    }

    // ── workouts_as_options ──────────────────────────────────────────────────

    fn icu_workout(name: &str, secs: Option<u32>, tss: Option<f32>) -> IntervalsWorkout {
        IntervalsWorkout {
            id: 1,
            icu_id: "w1".into(),
            name: name.into(),
            description: String::new(),
            duration_secs: secs,
            tss,
        }
    }

    #[test]
    fn should_prefix_intervals_templates_so_they_are_distinguishable() {
        let opts = workouts_as_options(&[], &[icu_workout("Sweet Spot", Some(3600), Some(70.0))]);
        assert_eq!(opts.len(), 1);
        assert_eq!(opts[0].name, "[Intervals.icu] Sweet Spot");
        assert_eq!(opts[0].duration_mins, 60);
        assert_eq!(opts[0].category, "Intervals.icu");
    }

    #[test]
    fn should_assume_an_hour_for_a_template_with_no_duration() {
        let opts = workouts_as_options(&[], &[icu_workout("Untimed", None, None)]);
        assert_eq!(opts[0].duration_mins, 60);
        assert_eq!(opts[0].tss, 0.0);
    }

    // ── resolve_planned_workout ──────────────────────────────────────────────

    fn workout(name: &str, secs: u32) -> Workout {
        Workout {
            id: 1,
            name: name.into(),
            description: String::new(),
            duration_secs: secs,
            tss: 60.0,
            category: crate::data::workout::WorkoutCategory::Endurance,
            segments: Vec::new(),
        }
    }

    #[test]
    fn should_prefer_the_scheduled_workout_over_the_cached_suggestion() {
        let library = vec![workout("Sweet Spot", 3600)];
        let scheduled = workout("Threshold 2x20", 4800);
        let planned = resolve_planned_workout(Some(&scheduled), "Sweet Spot", &library)
            .expect("a scheduled workout is always planned");
        assert_eq!(planned.name, "Threshold 2x20");
        assert_eq!(planned.duration_mins, 80);
    }

    #[test]
    fn should_fall_back_to_the_cached_suggestion_on_an_empty_day() {
        // Otherwise the Morning Brief and the Coaching page contradict each other.
        let library = vec![workout("Sweet Spot", 3600)];
        let planned = resolve_planned_workout(None, "Sweet Spot", &library)
            .expect("the cached suggestion should stand in");
        assert_eq!(planned.name, "Sweet Spot");
        assert_eq!(planned.duration_mins, 60);
    }

    #[test]
    fn should_plan_nothing_when_the_day_is_genuinely_open() {
        assert!(resolve_planned_workout(None, "", &[workout("Sweet Spot", 3600)]).is_none());
        assert!(resolve_planned_workout(None, "   ", &[workout("Sweet Spot", 3600)]).is_none());
    }

    #[test]
    fn should_plan_nothing_when_the_suggestion_names_a_deleted_workout() {
        let library = vec![workout("Sweet Spot", 3600)];
        assert!(resolve_planned_workout(None, "Workout Since Deleted", &library).is_none());
    }

    // ── marker line handling ─────────────────────────────────────────────────

    #[test]
    fn should_remove_the_marker_line_from_the_displayed_reply() {
        let reply = "Ride easy today.\nRECOMMENDED_WORKOUT: Recovery Spin\n";
        assert_eq!(strip_recommended_line(reply), "Ride easy today.");
        assert_eq!(
            extract_recommended_workout(reply).as_deref(),
            Some("Recovery Spin")
        );
    }

    #[test]
    fn should_remove_an_indented_marker_line() {
        let reply = "Ride easy today.\n   RECOMMENDED_WORKOUT: Recovery Spin";
        assert_eq!(strip_recommended_line(reply), "Ride easy today.");
    }

    #[test]
    fn should_leave_a_reply_without_a_marker_untouched() {
        assert_eq!(strip_recommended_line("Just rest."), "Just rest.");
        assert_eq!(extract_recommended_workout("Just rest."), None);
    }

    // ── day_name_to_offset ───────────────────────────────────────────────────

    #[test]
    fn should_map_each_weekday_to_its_offset_from_monday() {
        assert_eq!(day_name_to_offset("Monday"), 0);
        assert_eq!(day_name_to_offset("Sunday"), 6);
        assert_eq!(day_name_to_offset("wednesday"), 2);
        assert_eq!(day_name_to_offset("THURSDAY"), 3);
    }

    #[test]
    fn should_fall_back_to_monday_for_an_unrecognised_day() {
        // A misplaced workout the rider can drag is better than a missing one.
        assert_eq!(day_name_to_offset("Someday"), 0);
        assert_eq!(day_name_to_offset(""), 0);
    }

    // ── capitalize_first ─────────────────────────────────────────────────────

    #[test]
    fn should_capitalise_only_the_first_character() {
        assert_eq!(capitalize_first("monday"), "Monday");
        assert_eq!(capitalize_first("mONDAY"), "MONDAY");
        assert_eq!(capitalize_first(""), "");
    }

    // ── format_program ───────────────────────────────────────────────────────

    fn entry(week: u32, day: &str, name: &str) -> ProgramEntry {
        ProgramEntry {
            week,
            day: day.into(),
            workout_name: name.into(),
        }
    }

    #[test]
    fn should_return_nothing_for_an_empty_program() {
        assert_eq!(format_program(&[], &[], &[]), "");
    }

    #[test]
    fn should_head_each_week_and_list_its_days() {
        let entries = vec![
            entry(1, "monday", "Endurance"),
            entry(1, "wednesday", "Threshold"),
            entry(2, "monday", "Endurance"),
        ];
        let out = format_program(&entries, &[], &[]);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "## Week 1");
        assert_eq!(lines[1], "- Monday — Endurance");
        assert_eq!(lines[2], "- Wednesday — Threshold");
        assert_eq!(lines[3], "", "weeks are separated by a blank line");
        assert_eq!(lines[4], "## Week 2");
    }

    #[test]
    fn should_annotate_an_intervals_template_with_its_duration() {
        // The coach names the workout with the prefix; the lookup has to strip
        // it to find the template again.
        let entries = vec![entry(1, "monday", "[Intervals.icu] Sweet Spot")];
        let icu = vec![icu_workout("Sweet Spot", Some(2700), None)];
        let out = format_program(&entries, &[], &icu);
        assert!(
            out.contains("(45 min) [Intervals.icu]"),
            "expected a duration note, got: {out}"
        );
    }

    #[test]
    fn should_leave_an_unknown_workout_unannotated() {
        let entries = vec![entry(1, "monday", "Something Invented")];
        let out = format_program(&entries, &[], &[]);
        assert_eq!(out.lines().nth(1), Some("- Monday — Something Invented"));
    }

    // ── parse_ai_sections ────────────────────────────────────────────────────

    #[test]
    fn should_split_a_numbered_reply_into_sections() {
        let text = "1. **Training Load**:\nYou are building well.\n\
                    2. Recovery\nSleep has been short.";
        let sections = parse_ai_sections(text);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].0, "**Training Load**");
        assert_eq!(sections[0].1, "You are building well.");
        assert_eq!(sections[1].0, "Recovery");
    }

    #[test]
    fn should_refuse_to_structure_a_reply_with_only_one_section() {
        // One section is not a list — the caller shows the reply verbatim.
        assert!(parse_ai_sections("1. Summary\nAll good.").is_empty());
    }

    #[test]
    fn should_return_nothing_for_unnumbered_prose() {
        assert!(parse_ai_sections("You had a solid week of riding.").is_empty());
    }

    #[test]
    fn should_drop_a_heading_that_has_no_body() {
        let text = "1. Empty\n2. Load\nBuilding well.\n3. Recovery\nSleep is short.";
        let sections = parse_ai_sections(text);
        assert_eq!(sections.len(), 2, "the bodiless heading is dropped");
        assert_eq!(sections[0].0, "Load");
        assert_eq!(sections[1].0, "Recovery");
    }

    #[test]
    fn should_return_nothing_when_dropping_empty_headings_leaves_only_one() {
        // The two-section minimum applies to what survives, not to what was
        // detected — so this falls back to showing the reply verbatim.
        assert!(parse_ai_sections("1. Empty\n2. Also empty\n3. Real\nWith content.").is_empty());
    }
}
