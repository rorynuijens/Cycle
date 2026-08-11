//! The page's primary action: "what should I ride today?", and what the coach
//! answered.
//!
//! A recommendation resolves to one of three things — a workout from the
//! built-in library, which can be started or scheduled here; a workout that
//! lives on Intervals.icu, which can only be described; or a name that matches
//! nothing, in which case the prose stands on its own.

use adw::prelude::*;
use chrono::{Local, NaiveDate};
use sqlx::SqlitePool;
use std::cell::RefCell;
use std::rc::Rc;

use crate::ai::context::ICU_PREFIX;
use crate::data::{athlete::AthleteProfile, db, workout::Workout};
use crate::ui::brief_store::{BriefState, BriefStatus, BriefStore};
use crate::ui::markdown::to_pango;
use crate::ui::widgets::workout_graph::WorkoutGraph;
use crate::ui::AiFailure;

/// How a built-in workout is summarised under its name.
fn library_detail(workout: &Workout) -> String {
    format!(
        "{} · {} min · TSS {:.0}",
        workout.category.label(),
        workout.duration_secs / 60,
        workout.tss
    )
}

/// How an Intervals.icu workout is summarised.
///
/// Duration and TSS are both optional there — the workout may never have been
/// given a structure — so each degrades on its own rather than reporting zero.
fn intervals_detail(workout: &db::IntervalsWorkout) -> String {
    let duration = workout
        .duration_secs
        .map(|s| format!("{} min", s / 60))
        .unwrap_or_else(|| "—".to_string());
    let tss = workout
        .tss
        .map(|t| format!(" · TSS {t:.0}"))
        .unwrap_or_default();
    format!("Intervals.icu · {duration}{tss} — open Intervals.icu to start this workout")
}

pub struct SuggestionCard {
    root: gtk::Box,
    response: gtk::Label,
    action_frame: gtk::Box,
    thumb: gtk::Box,
    title: gtk::Label,
    detail: gtk::Label,
    start_btn: gtk::Button,
    schedule_btn: gtk::Button,
    provenance: gtk::Label,
    refresh_btn: gtk::Button,
    /// The workout the action buttons act on — `None` whenever the
    /// recommendation is not something this app can start.
    suggested: Rc<RefCell<Option<Workout>>>,
    athlete: Rc<RefCell<AthleteProfile>>,
    workouts: Rc<Vec<Workout>>,
}

impl SuggestionCard {
    pub fn new(
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        athlete: Rc<RefCell<AthleteProfile>>,
        workouts: Rc<Vec<Workout>>,
        on_start_workout: Rc<dyn Fn(Workout)>,
        on_toast: Rc<dyn Fn(adw::Toast)>,
        brief_store: Rc<BriefStore>,
    ) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .build();

        // Training context and library sync live in Preferences (Athlete /
        // Integrations) — the coach reads both from the database at request
        // time either way.
        let header = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        let title_col = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .hexpand(true)
            .build();
        title_col.append(
            &gtk::Label::builder()
                .label("Today")
                .halign(gtk::Align::Start)
                .css_classes(["heading"])
                .tooltip_text(
                    "Your morning brief's read on today, written from your training load, \
                     recent sessions, goals and wellness. Edit your training context in \
                     Preferences → Athlete.",
                )
                .build(),
        );
        // Where this text came from, and whether anything has happened since.
        // Worded once in BriefState so all three cards say the same thing.
        let provenance = gtk::Label::builder()
            .label("")
            .halign(gtk::Align::Start)
            .css_classes(["caption", "dim-label"])
            .build();
        title_col.append(&provenance);
        header.append(&title_col);

        // One shared action: every card that shows a slice of the brief
        // refreshes the same brief, so they cannot drift apart again.
        let refresh_btn = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .css_classes(["flat"])
            .tooltip_text("Ask your coach to write today's brief again")
            .valign(gtk::Align::Center)
            .build();
        {
            let store = Rc::clone(&brief_store);
            refresh_btn.connect_clicked(move |_| store.refresh());
        }
        header.append(&refresh_btn);
        root.append(&header);

        let response = gtk::Label::builder()
            .label("Your morning brief will appear here.")
            .css_classes(["dim-label"])
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
        let response_frame = gtk::Box::builder()
            .css_classes(["card"])
            .orientation(gtk::Orientation::Vertical)
            .build();
        response_frame.append(&response);
        root.append(&response_frame);

        // The recommendation gets the workout-profile treatment — a WorkoutGraph
        // thumbnail, the same as the library rows.
        let card_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(14)
            .margin_end(14)
            .build();

        let top_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .build();
        let thumb = gtk::Box::builder().build();
        top_row.append(&thumb);

        let text_col = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .valign(gtk::Align::Center)
            .build();
        let title = gtk::Label::builder()
            .label("")
            .halign(gtk::Align::Start)
            .css_classes(["heading"])
            .build();
        text_col.append(&title);
        let detail = gtk::Label::builder()
            .label("")
            .halign(gtk::Align::Start)
            .css_classes(["dim-label"])
            .build();
        text_col.append(&detail);
        top_row.append(&text_col);
        card_box.append(&top_row);

        let btns_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .build();
        let start_btn = gtk::Button::builder()
            .label("Start")
            .css_classes(["pill"])
            .tooltip_text("Load this workout and start riding")
            .build();
        let schedule_btn = gtk::Button::builder()
            .label("Schedule")
            .css_classes(["pill"])
            .tooltip_text("Schedule this workout on the calendar")
            .build();
        btns_row.append(&start_btn);
        btns_row.append(&schedule_btn);
        card_box.append(&btns_row);

        let action_frame = gtk::Box::builder().css_classes(["card"]).build();
        action_frame.append(&card_box);
        action_frame.set_visible(false);
        root.append(&action_frame);

        let card = Self {
            root,
            response,
            action_frame,
            thumb,
            title,
            detail,
            start_btn,
            schedule_btn,
            provenance,
            refresh_btn,
            suggested: Rc::new(RefCell::new(None)),
            athlete,
            workouts,
        };

        card.connect_start(on_start_workout);
        card.connect_schedule(pool, rt_handle, on_toast);
        card
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// Render the daily brief's slice of today.
    ///
    /// The card shows the session the rider is actually doing: whichever
    /// workout the brief was written about, or the one it chose when no program
    /// owned the day. It never asks for anything itself — see `ai::brief`.
    pub fn apply_brief(&self, state: &BriefState, icu_workouts: &[db::IntervalsWorkout]) {
        self.provenance.set_label(state.provenance());
        self.refresh_btn.set_sensitive(state.can_refresh());

        let Some(brief) = &state.brief else {
            // Nothing yet — say why, and offer no actions.
            let message = match state.status {
                BriefStatus::Loading => "Asking your coach…",
                BriefStatus::NoApiKey => AiFailure::NoApiKey.message(),
                BriefStatus::Failed(failure) => failure.message(),
                _ => "Your morning brief will appear here.",
            };
            self.response.set_text(message);
            self.action_frame.set_visible(false);
            *self.suggested.borrow_mut() = None;
            return;
        };

        match brief.session_slice() {
            Some(prose) => {
                self.response.set_markup(&to_pango(prose));
                self.response.remove_css_class("dim-label");
            }
            None => self
                .response
                .set_text("Your morning brief will appear here."),
        }

        // A workout the brief chose outright, else the one it was written about.
        let Some(name) = brief
            .recommended_workout
            .as_deref()
            .or(brief.planned_workout.as_deref())
            .filter(|n| !n.trim().is_empty())
        else {
            self.action_frame.set_visible(false);
            *self.suggested.borrow_mut() = None;
            return;
        };

        // Whether this is the plan's own session or the coach's pick changes
        // what the rider is being told, so it changes the label.
        let heading = if brief.recommended_workout.is_some() {
            "Recommended"
        } else {
            "Today"
        };

        if let Some(workout) = self
            .workouts
            .iter()
            .find(|w| crate::ai::naming::names_match(&w.name, name))
        {
            let detail = library_detail(workout);
            self.show_library(workout, &format!("{heading}: {}", workout.name), &detail);
            return;
        }

        // The coach may name a workout from the rider's Intervals.icu library,
        // which this app can describe but not run.
        let lookup = name.strip_prefix(ICU_PREFIX).unwrap_or(name);
        if let Some(workout) = icu_workouts
            .iter()
            .find(|w| crate::ai::naming::names_match(&w.name, lookup))
        {
            self.show_unstartable(
                &format!("{heading}: {} [Intervals.icu]", workout.name),
                &intervals_detail(workout),
            );
            return;
        }

        // A name matching nothing — the prose still stands on its own, but
        // there must be no button offering to start something that is gone.
        self.show_unstartable(&format!("{heading}: {name}"), "");
    }

    /// Show a built-in workout, with the buttons that act on it.
    fn show_library(&self, workout: &Workout, title: &str, detail: &str) {
        self.title.set_label(title);
        self.detail.set_label(detail);
        self.set_thumb(Some(workout));
        *self.suggested.borrow_mut() = Some(workout.clone());
        self.start_btn.set_visible(true);
        self.schedule_btn.set_visible(true);
        self.action_frame.set_visible(true);
    }

    /// Show a recommendation this app cannot start — an Intervals.icu workout,
    /// or a cached name the library no longer has.
    fn show_unstartable(&self, title: &str, detail: &str) {
        self.title.set_label(title);
        self.detail.set_label(detail);
        self.set_thumb(None);
        *self.suggested.borrow_mut() = None;
        self.start_btn.set_visible(false);
        self.schedule_btn.set_visible(false);
        self.action_frame.set_visible(true);
    }

    /// Replace the thumbnail with `workout`'s profile drawing, or hide it.
    fn set_thumb(&self, workout: Option<&Workout>) {
        while let Some(child) = self.thumb.first_child() {
            self.thumb.remove(&child);
        }
        if let Some(workout) = workout {
            let graph = WorkoutGraph::new(workout, self.athlete.borrow().ftp_watts);
            graph.widget().set_content_width(120);
            graph.widget().set_content_height(56);
            graph.widget().set_valign(gtk::Align::Center);
            self.thumb.append(graph.widget());
        }
        self.thumb.set_visible(workout.is_some());
    }

    fn connect_start(&self, on_start_workout: Rc<dyn Fn(Workout)>) {
        let suggested = Rc::clone(&self.suggested);
        self.start_btn.connect_clicked(move |_| {
            let workout = suggested.borrow().clone();
            if let Some(workout) = workout {
                on_start_workout(workout);
            }
        });
    }

    fn connect_schedule(
        &self,
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        on_toast: Rc<dyn Fn(adw::Toast)>,
    ) {
        let suggested = Rc::clone(&self.suggested);
        self.schedule_btn.connect_clicked(move |btn| {
            let Some(workout) = suggested.borrow().clone() else {
                return;
            };

            let dialog = adw::AlertDialog::new(
                Some(&format!("Schedule: {}", workout.name)),
                Some("Pick a date to add this workout to your calendar."),
            );
            dialog.add_response("cancel", "Cancel");
            dialog.add_response("schedule", "Schedule");
            dialog.set_response_appearance("schedule", adw::ResponseAppearance::Suggested);
            dialog.set_default_response(Some("schedule"));
            dialog.set_close_response("cancel");

            let date_entry = adw::EntryRow::builder()
                .title("Date (YYYY-MM-DD)")
                .text(Local::now().date_naive().format("%Y-%m-%d").to_string())
                .input_hints(gtk::InputHints::NO_EMOJI)
                .build();
            let date_list = gtk::ListBox::builder()
                .css_classes(["boxed-list"])
                .selection_mode(gtk::SelectionMode::None)
                .build();
            date_list.append(&date_entry);
            dialog.set_extra_child(Some(&date_list));

            let pool = pool.clone();
            let rt_handle = rt_handle.clone();
            let on_toast = Rc::clone(&on_toast);
            let workout_id = workout.id;
            let workout_name = workout.name.clone();

            dialog.connect_response(None, move |_, response| {
                if response != "schedule" {
                    return;
                }
                let Ok(date) = NaiveDate::parse_from_str(date_entry.text().trim(), "%Y-%m-%d")
                else {
                    on_toast(
                        adw::Toast::builder()
                            .title("Invalid date — use YYYY-MM-DD format")
                            .timeout(3)
                            .build(),
                    );
                    return;
                };

                let date_str = date.format("%Y-%m-%d").to_string();
                let pool = pool.clone();
                let on_toast = Rc::clone(&on_toast);
                let workout_name = workout_name.clone();
                let date_for_msg = date_str.clone();
                crate::ui::spawn_to_main(
                    &rt_handle,
                    async move { db::schedule_workout(&pool, workout_id, &date_str, None).await },
                    move |res| match res {
                        Ok(_) => on_toast(
                            adw::Toast::builder()
                                .title(format!("Scheduled {workout_name} on {date_for_msg}"))
                                .timeout(4)
                                .build(),
                        ),
                        Err(e) => {
                            tracing::error!("schedule_workout failed: {e}");
                            on_toast(
                                adw::Toast::builder()
                                    .title("Failed to schedule workout")
                                    .timeout(3)
                                    .build(),
                            );
                        }
                    },
                );
            });

            dialog.present(Some(btn));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::workout::WorkoutCategory;

    fn icu_workout(duration_secs: Option<u32>, tss: Option<f32>) -> db::IntervalsWorkout {
        db::IntervalsWorkout {
            id: 1,
            icu_id: "w1".into(),
            name: "Sweet Spot 3x15".into(),
            description: String::new(),
            duration_secs,
            tss,
        }
    }

    #[test]
    fn should_summarise_a_library_workout() {
        let workout = Workout {
            tss: 82.4,
            duration_secs: 3600,
            category: WorkoutCategory::Threshold,
            ..Workout::sample_threshold()
        };
        let detail = library_detail(&workout);
        assert!(detail.contains("60 min"), "got {detail}");
        assert!(detail.contains("TSS 82"), "got {detail}");
        assert!(detail.starts_with(WorkoutCategory::Threshold.label()));
    }

    #[test]
    fn should_round_a_library_workouts_tss_to_a_whole_number() {
        let workout = Workout {
            tss: 99.6,
            ..Workout::sample_threshold()
        };
        assert!(library_detail(&workout).contains("TSS 100"));
    }

    #[test]
    fn should_summarise_an_intervals_workout() {
        let detail = intervals_detail(&icu_workout(Some(2700), Some(65.0)));
        assert!(detail.contains("45 min"), "got {detail}");
        assert!(detail.contains("TSS 65"), "got {detail}");
        assert!(detail.starts_with("Intervals.icu · "));
    }

    #[test]
    fn should_dash_an_intervals_workout_with_no_duration() {
        // A workout on Intervals.icu may never have been given a structure.
        let detail = intervals_detail(&icu_workout(None, Some(65.0)));
        assert!(
            detail.contains("· — ·") || detail.contains("· —"),
            "got {detail}"
        );
        assert!(detail.contains("TSS 65"));
    }

    #[test]
    fn should_omit_tss_from_an_intervals_workout_that_has_none() {
        let detail = intervals_detail(&icu_workout(Some(2700), None));
        assert!(!detail.contains("TSS"), "got {detail}");
        assert!(detail.contains("45 min"));
    }

    #[test]
    fn should_say_an_intervals_workout_cannot_be_started_here() {
        // The app can describe these but not run them, and the card's buttons
        // stay hidden — the text is the only thing telling the rider why.
        assert!(intervals_detail(&icu_workout(Some(600), None)).contains("open Intervals.icu"));
    }
}
