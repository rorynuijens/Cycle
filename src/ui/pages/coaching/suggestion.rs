//! The page's primary action: "what should I ride today?", and what the coach
//! answered.
//!
//! A recommendation resolves to one of three things — a workout from the
//! built-in library, which can be started or scheduled here; a workout that
//! lives on Intervals.icu, which can only be described; or a name that matches
//! nothing, in which case the prose stands on its own.

use adw::prelude::*;
use chrono::{Duration as CDuration, Local, NaiveDate};
use gtk::glib;
use sqlx::SqlitePool;
use std::cell::RefCell;
use std::rc::Rc;

use crate::ai::coach::{build_prompt, get_suggestion, RecentSession, TrainingContext};
use crate::ai::context::{
    build_recent_session, extract_recommended_workout, icu_activity_to_recent_session,
    strip_recommended_line, wellness_snapshots, workouts_as_options, ICU_PREFIX,
};
use crate::data::{athlete::AthleteProfile, db, keystore, workout::Workout};
use crate::training::fitness::compute_load_metrics;
use crate::ui::markdown::to_pango;
use crate::ui::widgets::workout_graph::WorkoutGraph;
use crate::ui::AiFailure;

use super::data::{load_suggestion_prompt_data, SuggestionPromptData};

/// How far back the suggestion prompt summarises recent training.
const RECENT_TRAINING_WEEKS: i64 = 4;

/// Most recent sessions described to the coach.
const MAX_RECENT_SESSIONS: usize = 10;

const NO_API_KEY: &str = "No AI provider key configured. Enter your API key in \
                          Preferences → Integrations.";

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
        header.append(
            &gtk::Label::builder()
                .label("What should I ride today?")
                .halign(gtk::Align::Start)
                .hexpand(true)
                .css_classes(["heading"])
                .tooltip_text(
                    "The AI Coach analyses your training load, recent sessions, goals, and \
                     athlete profile to recommend a specific workout from your library. \
                     Edit your training context in Preferences → Athlete.",
                )
                .build(),
        );
        let spinner = gtk::Spinner::new();
        spinner.set_visible(false);
        header.append(&spinner);
        let get_btn = gtk::Button::builder()
            .label("Get Suggestion")
            .css_classes(["pill", "suggested-action"])
            .tooltip_text("Ask the AI Coach for a personalised workout suggestion")
            .valign(gtk::Align::Center)
            .build();
        header.append(&get_btn);
        root.append(&header);

        let response = gtk::Label::builder()
            .label(
                "Select \"Get Suggestion\" to receive a personalised workout recommendation \
                 from the AI Coach.",
            )
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
            suggested: Rc::new(RefCell::new(None)),
            athlete,
            workouts,
        };

        card.connect_get(&get_btn, &spinner, pool.clone(), rt_handle.clone());
        card.connect_start(on_start_workout);
        card.connect_schedule(pool, rt_handle, on_toast);
        card
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// Restore the suggestion cached from a previous session.
    ///
    /// The action buttons come back only for built-in workouts: a cached name
    /// that no longer matches the library must not offer a Start button that
    /// would do nothing.
    pub fn restore_cached(&self, response: &str, name: &str, detail: &str) {
        if response.trim().is_empty() {
            return;
        }
        self.response.set_markup(&to_pango(response));
        self.response.remove_css_class("dim-label");

        if name.is_empty() {
            return;
        }
        let title = format!("Recommended: {name}");
        match self
            .workouts
            .iter()
            .find(|w| crate::ai::naming::names_match(&w.name, name))
        {
            Some(workout) => self.show_library(workout, &title, detail),
            None => self.show_unstartable(&title, detail),
        }
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

    /// Resolve the coach's reply against both libraries and show the result.
    /// Returns `(name, detail)` to cache, empty when nothing resolved.
    fn apply_reply(&self, text: &str, icu_workouts: &[db::IntervalsWorkout]) -> (String, String) {
        let recommended = extract_recommended_workout(text);
        let prose = strip_recommended_line(text);
        self.response.set_markup(&to_pango(&prose));
        self.response.remove_css_class("dim-label");

        let Some(name) = recommended else {
            return (String::new(), String::new());
        };

        if let Some(workout) = self
            .workouts
            .iter()
            .find(|w| crate::ai::naming::names_match(&w.name, &name))
        {
            let detail = library_detail(workout);
            self.show_library(workout, &format!("Recommended: {}", workout.name), &detail);
            return (workout.name.clone(), detail);
        }

        // The coach may name a workout from the rider's Intervals.icu library,
        // which this app can describe but not run.
        let lookup = name.strip_prefix(ICU_PREFIX).unwrap_or(&name);
        if let Some(workout) = icu_workouts
            .iter()
            .find(|w| crate::ai::naming::names_match(&w.name, lookup))
        {
            let detail = intervals_detail(workout);
            self.show_unstartable(
                &format!("Recommended: {} [Intervals.icu]", workout.name),
                &detail,
            );
            return (format!("{ICU_PREFIX}{}", workout.name), detail);
        }

        // A name matching nothing: the prose still stands on its own.
        (name, String::new())
    }

    /// Show a plain status line — progress, or why a request produced nothing.
    fn set_status(&self, text: &str) {
        self.response.set_text(text);
        self.response.remove_css_class("dim-label");
    }

    fn connect_get(
        &self,
        button: &gtk::Button,
        spinner: &gtk::Spinner,
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
    ) {
        let card = self.clone_handles();
        let spinner = spinner.clone();
        let athlete = Rc::clone(&self.athlete);
        let workouts = Rc::clone(&self.workouts);

        button.connect_clicked(move |btn| {
            let api_key = match keystore::get_secret(keystore::KEY_ANTHROPIC) {
                Ok(Some(k)) if !k.trim().is_empty() => k,
                _ => {
                    card.set_status(NO_API_KEY);
                    card.action_frame.set_visible(false);
                    return;
                }
            };

            // Read the !Send shared state on the main thread before spawning.
            let profile = athlete.borrow().clone();
            let ftp_watts = profile.ftp_watts;
            let library: Vec<Workout> = (*workouts).clone();

            btn.set_sensitive(false);
            spinner.set_visible(true);
            spinner.start();
            card.set_status("Asking the AI Coach for a suggestion…");
            card.action_frame.set_visible(false);
            *card.suggested.borrow_mut() = None;

            let (tx, rx) =
                async_channel::bounded::<Result<(String, Vec<db::IntervalsWorkout>), AiFailure>>(1);
            let pool_task = pool.clone();
            // All DB reads + prompt assembly + the network call run off the main
            // thread (CLAUDE.md §2.3). icu_workouts comes back with the reply so
            // the handler can still match an Intervals.icu recommendation.
            rt_handle.spawn(async move {
                let today = Local::now().date_naive();
                let SuggestionPromptData {
                    athlete_ctx,
                    records,
                    intervals_pairs,
                    icu_activities,
                    goals,
                    icu_workouts,
                    wellness,
                    time_off,
                } = match load_suggestion_prompt_data(&pool_task, today).await {
                    Ok(data) => data,
                    Err(e) => {
                        tracing::error!("Could not read training history to suggest: {e}");
                        let _ = tx.send(Err(AiFailure::DataUnavailable)).await;
                        return;
                    }
                };

                let metrics = compute_load_metrics(&records, &intervals_pairs, ftp_watts, today);
                let ctx = TrainingContext {
                    athlete: profile,
                    ctl: metrics.ctl,
                    atl: metrics.atl,
                    tsb: metrics.tsb(),
                    recent_sessions: recent_sessions(&records, &icu_activities, ftp_watts, today),
                    goals,
                    athlete_context: athlete_ctx,
                    workout_options: workouts_as_options(&library, &icu_workouts),
                    wellness: wellness_snapshots(&wellness),
                    time_off_dates: time_off
                        .into_iter()
                        .map(|e| e.date.format("%Y-%m-%d").to_string())
                        .collect(),
                };

                let result = get_suggestion(&api_key, &build_prompt(&ctx), 1024)
                    .await
                    .map(|text| (text, icu_workouts))
                    .map_err(|e| {
                        tracing::error!("AI coaching request failed: {e}");
                        AiFailure::Request
                    });
                let _ = tx.send(result).await;
            });

            let card = card.clone_handles();
            let btn = btn.clone();
            let spinner = spinner.clone();
            let pool = pool.clone();
            let rt_handle = rt_handle.clone();
            glib::MainContext::default().spawn_local(async move {
                if let Ok(result) = rx.recv().await {
                    match result {
                        Ok((text, icu_workouts)) => {
                            let prose = strip_recommended_line(&text);
                            let (name, detail) = card.apply_reply(&text, &icu_workouts);
                            // Cached as raw text, not markup — the dashboard and
                            // calendar render it themselves.
                            cache_suggestion(&pool, &rt_handle, prose, name, detail);
                        }
                        Err(failure) => card.set_status(failure.message()),
                    }
                }
                spinner.stop();
                spinner.set_visible(false);
                btn.set_sensitive(true);
            });
        });
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
                    async move { db::schedule_workout(&pool, workout_id, &date_str).await },
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

    /// A second handle on the same widgets, for moving into a callback.
    ///
    /// Everything here is a refcounted handle, so this shares the card rather
    /// than copying it — the clone drives the same widgets.
    fn clone_handles(&self) -> Self {
        Self {
            root: self.root.clone(),
            response: self.response.clone(),
            action_frame: self.action_frame.clone(),
            thumb: self.thumb.clone(),
            title: self.title.clone(),
            detail: self.detail.clone(),
            start_btn: self.start_btn.clone(),
            schedule_btn: self.schedule_btn.clone(),
            suggested: Rc::clone(&self.suggested),
            athlete: Rc::clone(&self.athlete),
            workouts: Rc::clone(&self.workouts),
        }
    }
}

/// The rides the coach is told about: recent local sessions and synced
/// activities, newest first and capped so the prompt stays a summary.
fn recent_sessions(
    records: &[db::SessionSummary],
    icu_activities: &[db::IntervalsActivity],
    ftp_watts: u32,
    today: NaiveDate,
) -> Vec<RecentSession> {
    let since = today - CDuration::weeks(RECENT_TRAINING_WEEKS);
    let mut recent: Vec<RecentSession> = records
        .iter()
        .filter(|r| r.started_at.with_timezone(&Local).date_naive() >= since)
        .map(|r| build_recent_session(r, ftp_watts))
        .collect();
    for activity in icu_activities.iter().filter(|a| a.date >= since) {
        recent.push(icu_activity_to_recent_session(activity));
    }
    recent.sort_by(|a, b| b.date.cmp(&a.date));
    recent.truncate(MAX_RECENT_SESSIONS);
    recent
}

/// Persist the suggestion so the dashboard and calendar can show it too.
fn cache_suggestion(
    pool: &SqlitePool,
    rt_handle: &tokio::runtime::Handle,
    response: String,
    name: String,
    detail: String,
) {
    let pool = pool.clone();
    rt_handle.spawn(async move {
        for (key, value) in [
            ("ai.suggestion_response", response),
            ("ai.suggestion_workout_name", name),
            ("ai.suggestion_workout_detail", detail),
        ] {
            if let Err(e) = db::set_setting(&pool, key, &value).await {
                tracing::error!("Could not cache {key}: {e}");
            }
        }
    });
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
