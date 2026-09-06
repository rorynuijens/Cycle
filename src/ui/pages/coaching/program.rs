//! The multi-week training program: what to build, and putting it on the calendar.

use adw::prelude::*;
use chrono::{Datelike, Duration as CDuration, Local, NaiveDate};
use gtk::glib;
use sqlx::SqlitePool;
use std::cell::RefCell;
use std::rc::Rc;

use crate::ai::coach::{
    build_program_prompt, get_suggestion, parse_program_response, ProgramContext, ProgramEntry,
};
use crate::ai::context::{drop_time_off_days, entry_date, format_program, workouts_as_options};
use crate::data::{athlete::AthleteProfile, db, keystore, workout::Workout};
use crate::training::fitness::compute_load_metrics;
use crate::ui::markdown::to_pango;
use crate::ui::AiFailure;

use super::data::{load_program_prompt_data, ProgramPromptData};

/// The days of the week, as shown and as the prompt names them.
const DAYS: [(&str, &str); 7] = [
    ("Mon", "monday"),
    ("Tue", "tuesday"),
    ("Wed", "wednesday"),
    ("Thu", "thursday"),
    ("Fri", "friday"),
    ("Sat", "saturday"),
    ("Sun", "sunday"),
];

/// Selected by default — the classic three-day week.
const DEFAULT_DAYS: [usize; 3] = [0, 2, 4];

/// Weeks generated when the rider asks for no fixed end date.
const OPEN_ENDED_WEEKS: u32 = 8;

const NO_API_KEY: &str = "No AI provider key configured. Enter your API key in \
                          Preferences → Integrations.";

/// The Monday of the week containing `date`.
///
/// Programs are laid out in whole weeks, so a plan started mid-week still
/// begins on that week's Monday rather than shifting every later week.
pub(super) fn week_start(date: NaiveDate) -> NaiveDate {
    date - CDuration::days(date.weekday().num_days_from_monday() as i64)
}

pub struct ProgramSection {
    root: gtk::Box,
    output: gtk::Label,
    output_frame: gtk::Box,
    schedule_btn: gtk::Button,
    day_toggles: Vec<gtk::ToggleButton>,
    months_row: adw::SpinRow,
    open_ended_row: adw::SwitchRow,
    entries: Rc<RefCell<Vec<ProgramEntry>>>,
    /// The Monday the coach was told week 1 would land on. The schedule dialog
    /// offers it back rather than defaulting to today, so a plan built around a
    /// holiday in week six is scheduled onto the weeks it was planned for.
    plan_start: Rc<RefCell<Option<NaiveDate>>>,
    workouts: Rc<Vec<Workout>>,
}

impl ProgramSection {
    pub fn new(
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        athlete: Rc<RefCell<AthleteProfile>>,
        workouts: Rc<Vec<Workout>>,
        on_toast: Rc<dyn Fn(adw::Toast)>,
    ) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .build();

        let header = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        header.append(
            &gtk::Label::builder()
                .label("Program")
                .halign(gtk::Align::Start)
                .hexpand(true)
                .css_classes(["heading"])
                .tooltip_text(
                    "Build a multi-week structured plan from your goals and current \
                     fitness. The AI Coach applies progressive overload, selects \
                     workouts from your library, and includes recovery weeks.",
                )
                .build(),
        );
        let spinner = gtk::Spinner::new();
        spinner.set_visible(false);
        header.append(&spinner);
        let build_btn = gtk::Button::builder()
            .label("Build Program")
            .css_classes(["pill"])
            .tooltip_text("Ask the AI Coach to generate a structured training program")
            .valign(gtk::Align::Center)
            .build();
        header.append(&build_btn);
        root.append(&header);

        root.append(
            &gtk::Label::builder()
                .label("Training days")
                .halign(gtk::Align::Start)
                .css_classes(["caption-heading", "dim-label"])
                .build(),
        );

        // Linked toggle group (the calendar's Week|Month pattern, multi-select)
        // — native pressed state, no CSS hacks.
        let days_row = gtk::Box::builder().css_classes(["linked"]).build();
        let day_toggles: Vec<gtk::ToggleButton> = DAYS
            .iter()
            .map(|(label, _)| {
                let toggle = gtk::ToggleButton::builder()
                    .label(*label)
                    .tooltip_text(format!("Train on {label}"))
                    .build();
                days_row.append(&toggle);
                toggle
            })
            .collect();
        for i in DEFAULT_DAYS {
            day_toggles[i].set_active(true);
        }
        root.append(&days_row);

        let months_adj = gtk::Adjustment::new(3.0, 1.0, 24.0, 1.0, 3.0, 0.0);
        let months_row = adw::SpinRow::new(Some(&months_adj), 1.0, 0);
        months_row.set_title("Duration (months)");
        months_row.set_tooltip_text(Some("Number of months for the training program"));

        let open_ended_row = adw::SwitchRow::builder()
            .title("Open-ended")
            .subtitle(format!(
                "Generate {OPEN_ENDED_WEEKS} weeks without a fixed end date"
            ))
            .tooltip_text("Build a program without a fixed end date")
            .build();

        let duration_list = gtk::ListBox::builder()
            .css_classes(["boxed-list"])
            .selection_mode(gtk::SelectionMode::None)
            .build();
        duration_list.append(&months_row);
        duration_list.append(&open_ended_row);
        root.append(&duration_list);

        // A duration means nothing once there is no end date to count towards.
        let months_for_toggle = months_row.clone();
        open_ended_row.connect_active_notify(move |row| {
            months_for_toggle.set_sensitive(!row.is_active());
        });

        let output = gtk::Label::builder()
            .label("")
            .halign(gtk::Align::Start)
            .valign(gtk::Align::Start)
            .wrap(true)
            .selectable(true)
            .xalign(0.0)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(14)
            .margin_end(14)
            .build();
        let output_frame = gtk::Box::builder()
            .css_classes(["card"])
            .orientation(gtk::Orientation::Vertical)
            .build();
        output_frame.append(&output);
        output_frame.set_visible(false);
        root.append(&output_frame);

        let schedule_btn = gtk::Button::builder()
            .label("Schedule to Calendar")
            .css_classes(["pill"])
            .tooltip_text("Add all program workouts to your calendar starting from a chosen date")
            .halign(gtk::Align::Start)
            .visible(false)
            .build();
        root.append(&schedule_btn);

        let section = Self {
            root,
            output,
            output_frame,
            schedule_btn,
            day_toggles,
            months_row,
            open_ended_row,
            entries: Rc::new(RefCell::new(Vec::new())),
            plan_start: Rc::new(RefCell::new(None)),
            workouts,
        };

        section.connect_build(
            &build_btn,
            &spinner,
            pool.clone(),
            rt_handle.clone(),
            athlete,
        );
        section.connect_schedule(pool, rt_handle, on_toast);
        section
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// The days the rider ticked, named as the prompt expects.
    fn selected_days(&self) -> Vec<String> {
        self.day_toggles
            .iter()
            .zip(DAYS.iter())
            .filter(|(toggle, _)| toggle.is_active())
            .map(|(_, (_, value))| (*value).to_string())
            .collect()
    }

    /// How many weeks to plan, or `None` when the rider wants no end date.
    fn requested_weeks(&self) -> Option<u32> {
        if self.open_ended_row.is_active() {
            None
        } else {
            Some((self.months_row.value() as u32) * 4)
        }
    }

    /// Show a message in place of a program.
    fn set_status(&self, text: &str) {
        self.output_frame.set_visible(true);
        self.output.set_text(text);
    }

    fn connect_build(
        &self,
        button: &gtk::Button,
        spinner: &gtk::Spinner,
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        athlete: Rc<RefCell<AthleteProfile>>,
    ) {
        let section = self.clone_handles();
        let spinner = spinner.clone();

        button.connect_clicked(move |btn| {
            let api_key = match keystore::get_secret(keystore::KEY_ANTHROPIC) {
                Ok(Some(k)) if !k.trim().is_empty() => k,
                _ => {
                    section.set_status(NO_API_KEY);
                    return;
                }
            };

            let training_days = section.selected_days();
            if training_days.is_empty() {
                section.set_status("Please select at least one training day.");
                return;
            }
            let num_weeks = section.requested_weeks();

            // Fixed here, before the request, rather than at scheduling time.
            // The coach answers in (week, day) pairs, so it can only be told to
            // avoid a date if the calendar those weeks land on is already
            // decided — see the PLANNED TIME OFF block in the prompt.
            let start_monday = week_start(Local::now().date_naive());
            *section.plan_start.borrow_mut() = Some(start_monday);
            // The last day the plan can reach, so time off is read that far.
            let through =
                start_monday + CDuration::days(num_weeks.unwrap_or(OPEN_ENDED_WEEKS) as i64 * 7);

            // Read the !Send shared state on the main thread before spawning.
            let profile = athlete.borrow().clone();
            let ftp_watts = profile.ftp_watts;
            let library: Vec<Workout> = (*section.workouts).clone();

            btn.set_sensitive(false);
            spinner.set_visible(true);
            spinner.start();
            section.set_status("Building your training program…");
            section.schedule_btn.set_visible(false);
            section.entries.borrow_mut().clear();

            let (tx, rx) =
                async_channel::bounded::<Result<(String, Vec<db::IntervalsWorkout>), AiFailure>>(1);
            let pool_task = pool.clone();
            // All DB reads + prompt assembly + the network call run off the main
            // thread (CLAUDE.md §2.3). icu_workouts comes back with the reply so
            // the handler can format a program naming Intervals.icu workouts.
            rt_handle.spawn(async move {
                let today = Local::now().date_naive();
                let ProgramPromptData {
                    athlete_ctx,
                    goals,
                    records,
                    intervals_pairs,
                    icu_workouts,
                    wellness: _,
                    time_off,
                } = match load_program_prompt_data(&pool_task, today, through).await {
                    Ok(data) => data,
                    Err(e) => {
                        tracing::error!("Could not read training history to plan: {e}");
                        let _ = tx.send(Err(AiFailure::DataUnavailable)).await;
                        return;
                    }
                };

                let metrics = compute_load_metrics(&records, &intervals_pairs, ftp_watts, today);
                let ctx = ProgramContext {
                    athlete: profile,
                    ctl: metrics.ctl,
                    tsb: metrics.tsb(),
                    goals,
                    athlete_context: athlete_ctx,
                    workout_options: workouts_as_options(&library, &icu_workouts),
                    training_days,
                    num_weeks,
                    start_monday,
                    time_off: time_off.iter().map(|t| t.date).collect(),
                };

                let result = get_suggestion(&api_key, &build_program_prompt(&ctx), 2800)
                    .await
                    .map(|text| (text, icu_workouts))
                    .map_err(|e| {
                        tracing::error!("AI coaching request failed: {e}");
                        AiFailure::Request
                    });
                let _ = tx.send(result).await;
            });

            let section = section.clone_handles();
            let btn = btn.clone();
            let spinner = spinner.clone();
            glib::MainContext::default().spawn_local(async move {
                if let Ok(result) = rx.recv().await {
                    match result {
                        Ok((text, icu_workouts)) => {
                            let entries = parse_program_response(&text);
                            if entries.is_empty() {
                                section.set_status(
                                    "Could not parse the program response. Please try again.",
                                );
                            } else {
                                let display =
                                    format_program(&entries, &section.workouts, &icu_workouts);
                                section.output.set_markup(&to_pango(&display));
                                section.output_frame.set_visible(true);
                                *section.entries.borrow_mut() = entries;
                                section.schedule_btn.set_visible(true);
                            }
                        }
                        Err(failure) => section.set_status(failure.message()),
                    }
                }
                spinner.stop();
                spinner.set_visible(false);
                btn.set_sensitive(true);
            });
        });
    }

    fn connect_schedule(
        &self,
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        on_toast: Rc<dyn Fn(adw::Toast)>,
    ) {
        let entries = Rc::clone(&self.entries);
        let plan_start = Rc::clone(&self.plan_start);
        let workouts = Rc::clone(&self.workouts);

        let days = self.day_toggles.clone();

        self.schedule_btn.connect_clicked(move |btn| {
            let entries = entries.borrow().clone();
            if entries.is_empty() {
                return;
            }
            // The week the coach planned around. It is always set by the time
            // this button is visible; today's week is only a floor to stand on.
            let planned_start = plan_start
                .borrow()
                .unwrap_or_else(|| week_start(Local::now().date_naive()));

            // Whether a program is already being followed decides what this
            // dialog has to say, so it is read before the dialog is built —
            // the rider is told they are replacing a plan before they agree
            // to it, not after.
            let pool = pool.clone();
            let rt_handle = rt_handle.clone();
            let on_toast = Rc::clone(&on_toast);
            let workouts = Rc::clone(&workouts);
            let training_days = days
                .iter()
                .zip(DAYS.iter())
                .filter(|(toggle, _)| toggle.is_active())
                .map(|(_, (_, value))| *value)
                .collect::<Vec<_>>()
                .join(",");
            let btn = btn.clone();
            let pool_for_check = pool.clone();

            crate::ui::spawn_to_main(
                &rt_handle.clone(),
                async move { db::active_program(&pool_for_check).await },
                move |existing| {
                    let existing = match existing {
                        Ok(p) => p,
                        Err(e) => {
                            // Scheduling on top of an unknown state could
                            // silently double a plan, so it does not proceed.
                            tracing::error!("Could not check for an existing program: {e}");
                            on_toast(
                                adw::Toast::builder()
                                    .title(
                                        "Could not read your current program — nothing scheduled",
                                    )
                                    .timeout(5)
                                    .build(),
                            );
                            return;
                        }
                    };

                    // Naming the planned week matters: the coach placed its
                    // rest days against *these* dates, so a different start
                    // slides every week onto a different part of the calendar.
                    let planned = format!(
                        "This program was built for the week beginning {}. Starting it \
                         elsewhere shifts every week; planned time off is re-checked \
                         against wherever it lands.",
                        planned_start.format("%-d %B %Y")
                    );
                    let body = match &existing {
                        Some(_) => format!(
                            "{planned}\n\nYou are already following a program. Its remaining \
                             sessions will be replaced by this one; rides you have already \
                             done are kept."
                        ),
                        None => planned,
                    };

                    let dialog = adw::AlertDialog::new(Some("Schedule Program"), Some(&body));
                    dialog.add_response("cancel", "Cancel");
                    dialog.add_response(
                        "schedule",
                        if existing.is_some() {
                            "Replace"
                        } else {
                            "Schedule"
                        },
                    );
                    dialog.set_response_appearance("schedule", adw::ResponseAppearance::Suggested);
                    dialog.set_default_response(Some("schedule"));
                    dialog.set_close_response("cancel");

                    let date_entry = adw::EntryRow::builder()
                        .title("Start date (YYYY-MM-DD)")
                        .text(planned_start.format("%Y-%m-%d").to_string())
                        .input_hints(gtk::InputHints::NO_EMOJI)
                        .build();
                    let date_list = gtk::ListBox::builder()
                        .css_classes(["boxed-list"])
                        .selection_mode(gtk::SelectionMode::None)
                        .build();
                    date_list.append(&date_entry);
                    dialog.set_extra_child(Some(&date_list));

                    let replacing = existing.map(|p| p.id);
                    Self::connect_schedule_response(
                        &dialog,
                        date_entry,
                        entries,
                        workouts,
                        training_days,
                        replacing,
                        pool,
                        rt_handle,
                        on_toast,
                    );
                    dialog.present(Some(&btn));
                },
            );
        });
    }

    /// The half of scheduling that runs once the rider has picked a date.
    ///
    /// Split out so the caller stays readable: everything above it decides what
    /// the dialog should say, and everything here acts on the answer.
    #[allow(clippy::too_many_arguments)]
    fn connect_schedule_response(
        dialog: &adw::AlertDialog,
        date_entry: adw::EntryRow,
        entries: Vec<ProgramEntry>,
        workouts: Rc<Vec<Workout>>,
        training_days: String,
        replacing: Option<i64>,
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        on_toast: Rc<dyn Fn(adw::Toast)>,
    ) {
        {
            dialog.connect_response(None, move |_, response| {
                if response != "schedule" {
                    return;
                }
                let selected = NaiveDate::parse_from_str(date_entry.text().trim(), "%Y-%m-%d")
                    .unwrap_or_else(|_| Local::now().date_naive());
                let start_monday = week_start(selected);

                // Entries resolve to workout ids here, on the main thread, since
                // the library is held behind a non-Send Rc. The writes then run
                // on the tokio runtime over owned data.
                let mut to_schedule: Vec<(i64, NaiveDate)> = Vec::new();
                let mut skipped = 0u32;
                for entry in &entries {
                    let date = entry_date(start_monday, entry);
                    // Intervals.icu workouts carry no segments, so they can be
                    // planned but not scheduled; they count as skipped.
                    match workouts
                        .iter()
                        .find(|w| crate::ai::naming::names_match(&w.name, &entry.workout_name))
                    {
                        Some(w) => to_schedule.push((w.id, date)),
                        None => {
                            tracing::warn!(
                                "Workout '{}' not in library — skipped",
                                entry.workout_name
                            );
                            skipped += 1;
                        }
                    }
                }

                // The span the plan actually covers, for the time-off lookup.
                let span = to_schedule.iter().map(|(_, d)| *d).fold(
                    None::<(NaiveDate, NaiveDate)>,
                    |acc, d| {
                        Some(match acc {
                            None => (d, d),
                            Some((lo, hi)) => (lo.min(d), hi.max(d)),
                        })
                    },
                );

                // The program spans as many weeks as the coach actually
                // returned, which is what the rider will be held to — not the
                // number that was asked for.
                let weeks = entries.iter().map(|e| e.week.max(1)).max().unwrap_or(1);

                let pool = pool.clone();
                let on_toast = Rc::clone(&on_toast);
                let training_days = training_days.clone();
                crate::ui::spawn_to_main(
                    &rt_handle,
                    async move {
                        // Re-read rather than reusing what the prompt was built
                        // from: the rider may have booked time off since, and
                        // may have moved the start date in the dialog above.
                        // A failure here is not worth abandoning the plan for —
                        // the cost is a session on a day off, which the rider
                        // can delete.
                        let mut off_days = 0usize;
                        if let Some((first, last)) = span {
                            match db::load_time_off_between(
                                &pool,
                                &first.format("%Y-%m-%d").to_string(),
                                &last.format("%Y-%m-%d").to_string(),
                            )
                            .await
                            {
                                Ok(entries) => {
                                    let off = entries.into_iter().map(|t| t.date).collect();
                                    off_days = drop_time_off_days(&mut to_schedule, &off);
                                }
                                Err(e) => {
                                    tracing::warn!("could not read planned time off: {e}")
                                }
                            }
                        }

                        // A rider away for the whole block would otherwise have
                        // their current program retired and replaced by an empty
                        // one. Nothing has been written yet, so nothing is lost
                        // by stopping here.
                        if to_schedule.is_empty() {
                            return Ok((0, 0, off_days));
                        }

                        // Retiring the old plan first means a failure here
                        // leaves the rider following one program, not two.
                        if let Some(old) = replacing {
                            if let Err(e) =
                                db::clear_future_sessions(&pool, old, start_monday).await
                            {
                                tracing::error!("clearing the previous program: {e}");
                                return Err(());
                            }
                            if let Err(e) = db::deactivate_program(&pool, old).await {
                                tracing::error!("retiring the previous program: {e}");
                                return Err(());
                            }
                        }

                        let program_id = match db::save_program(
                            &pool,
                            start_monday,
                            weeks,
                            &training_days,
                        )
                        .await
                        {
                            Ok(id) => id,
                            Err(e) => {
                                tracing::error!("saving the program: {e}");
                                return Err(());
                            }
                        };

                        let mut scheduled = 0u32;
                        let mut failed = 0u32;
                        for (id, date) in to_schedule {
                            let date = date.format("%Y-%m-%d").to_string();
                            match db::schedule_workout(&pool, id, &date, Some(program_id)).await {
                                Ok(_) => scheduled += 1,
                                Err(e) => {
                                    tracing::error!("schedule_workout {id} on {date}: {e}");
                                    failed += 1;
                                }
                            }
                        }
                        Ok((scheduled, failed, off_days))
                    },
                    move |result| {
                        let msg = match result {
                            // A whole block on time off is not a failure, and
                            // must not read like one.
                            Ok((0, _, off_days)) if off_days > 0 => {
                                "Nothing scheduled — every session fell on your planned time off"
                                    .to_string()
                            }
                            Ok((scheduled, failed, off_days)) => {
                                let missed = failed + skipped;
                                // Counted apart from the rest: a session left
                                // off because the rider is away is the plan
                                // working, not the plan failing.
                                let mut msg = format!("{scheduled} workouts added to calendar");
                                if missed > 0 {
                                    msg.push_str(&format!(" · {missed} skipped"));
                                }
                                if off_days > 0 {
                                    msg.push_str(&format!(" · {off_days} skipped for time off"));
                                }
                                msg
                            }
                            Err(()) => {
                                "Could not save the program — nothing was scheduled".to_string()
                            }
                        };
                        on_toast(adw::Toast::builder().title(msg).timeout(5).build());
                    },
                );
            });
        }
    }

    /// A second handle on the same widgets, for moving into a callback.
    fn clone_handles(&self) -> Self {
        Self {
            root: self.root.clone(),
            output: self.output.clone(),
            output_frame: self.output_frame.clone(),
            schedule_btn: self.schedule_btn.clone(),
            day_toggles: self.day_toggles.clone(),
            months_row: self.months_row.clone(),
            open_ended_row: self.open_ended_row.clone(),
            entries: Rc::clone(&self.entries),
            plan_start: Rc::clone(&self.plan_start),
            workouts: Rc::clone(&self.workouts),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::context::day_name_to_offset;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("hardcoded valid date")
    }

    fn entry(week: u32, day: &str) -> ProgramEntry {
        ProgramEntry {
            week,
            day: day.into(),
            workout_name: "Sweet Spot".into(),
        }
    }

    #[test]
    fn should_leave_a_monday_start_where_it_is() {
        let monday = date(2026, 8, 3);
        assert_eq!(week_start(monday), monday);
    }

    #[test]
    fn should_wind_a_midweek_start_back_to_monday() {
        // Thursday 6 Aug 2026 belongs to the week beginning Monday the 3rd.
        assert_eq!(week_start(date(2026, 8, 6)), date(2026, 8, 3));
    }

    #[test]
    fn should_wind_a_sunday_start_back_to_the_same_week() {
        // Sunday closes its week rather than opening the next one.
        assert_eq!(week_start(date(2026, 8, 9)), date(2026, 8, 3));
    }

    #[test]
    fn should_put_the_first_week_on_the_starting_monday() {
        let monday = date(2026, 8, 3);
        assert_eq!(entry_date(monday, &entry(1, "monday")), monday);
    }

    #[test]
    fn should_offset_later_days_within_the_week() {
        let monday = date(2026, 8, 3);
        assert_eq!(entry_date(monday, &entry(1, "wednesday")), date(2026, 8, 5));
        assert_eq!(entry_date(monday, &entry(1, "sunday")), date(2026, 8, 9));
    }

    #[test]
    fn should_advance_a_whole_week_per_program_week() {
        let monday = date(2026, 8, 3);
        assert_eq!(entry_date(monday, &entry(2, "monday")), date(2026, 8, 10));
        assert_eq!(entry_date(monday, &entry(4, "friday")), date(2026, 8, 28));
    }

    #[test]
    fn should_treat_week_zero_as_the_first_week() {
        // Weeks are 1-based in the reply; a 0 would otherwise schedule the
        // entry a week before the program starts.
        let monday = date(2026, 8, 3);
        assert_eq!(entry_date(monday, &entry(0, "monday")), monday);
    }

    #[test]
    fn should_put_an_unrecognised_day_on_the_monday() {
        // Rather than dropping the session out of the plan entirely.
        let monday = date(2026, 8, 3);
        assert_eq!(entry_date(monday, &entry(1, "someday")), monday);
    }

    #[test]
    fn should_cross_a_month_boundary_correctly() {
        let monday = date(2026, 8, 31);
        assert_eq!(entry_date(monday, &entry(2, "tuesday")), date(2026, 9, 8));
    }

    #[test]
    fn should_name_every_weekday_the_offsets_understand() {
        // The toggle values feed straight into day_name_to_offset, so a typo
        // here would silently schedule everything on a Monday.
        for (i, (_, value)) in DAYS.iter().enumerate() {
            assert_eq!(day_name_to_offset(value), i as u32, "for {value}");
        }
    }

    #[test]
    fn should_default_to_a_three_day_week() {
        assert_eq!(DEFAULT_DAYS.len(), 3);
        let names: Vec<&str> = DEFAULT_DAYS.iter().map(|&i| DAYS[i].0).collect();
        assert_eq!(names, vec!["Mon", "Wed", "Fri"]);
    }
}
