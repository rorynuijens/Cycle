use adw::prelude::*;
use gtk::glib;
use std::cell::RefCell;
use std::rc::Rc;

use crate::training::fitness::compute_load_metrics;
use crate::ui::markdown::to_pango;

use chrono::{Datelike, Duration as CDuration, Local, NaiveDate};
use sqlx::SqlitePool;

use crate::ai::coach::{
    build_program_prompt, build_prompt, get_suggestion, parse_program_response, ProgramContext,
    ProgramEntry, RecentSession, TrainingContext,
};
use crate::ai::context::{
    build_recent_session, day_name_to_offset, extract_recommended_workout, format_program,
    icu_activity_to_recent_session, strip_recommended_line, wellness_snapshots,
    workouts_as_options, ICU_PREFIX,
};
use crate::data::{athlete::AthleteProfile, db, keystore, workout::Workout};
use crate::ui::widgets::workout_graph::WorkoutGraph;
use crate::ui::AiFailure;

/// Days of wellness history sent with a coaching prompt.
const AI_WELLNESS_DAYS: u32 = 7;

/// How far ahead the suggestion prompt looks for planned time off.
const TIME_OFF_LOOKAHEAD_DAYS: i64 = 14;

/// How far back the suggestion prompt summarises recent training.
const RECENT_TRAINING_WEEKS: i64 = 4;

/// Most recent sessions described to the coach.
const MAX_RECENT_SESSIONS: usize = 10;

/// The page's own state: the rider's goals and the last cached suggestion.
struct CoachingData {
    goals: Vec<db::AthleteGoal>,
    cached_resp: String,
    cached_name: String,
    cached_detail: String,
}

/// Load the page's state off the GTK main thread (CLAUDE.md §2.3).
async fn load_coaching_data(pool: &SqlitePool) -> anyhow::Result<CoachingData> {
    Ok(CoachingData {
        goals: db::load_goals(pool).await?,
        cached_resp: db::get_setting(pool, "ai.suggestion_response")
            .await?
            .unwrap_or_default(),
        cached_name: db::get_setting(pool, "ai.suggestion_workout_name")
            .await?
            .unwrap_or_default(),
        cached_detail: db::get_setting(pool, "ai.suggestion_workout_detail")
            .await?
            .unwrap_or_default(),
    })
}

/// The training history behind a "what should I ride today?" prompt.
struct SuggestionPromptData {
    athlete_ctx: String,
    records: Vec<db::SessionSummary>,
    intervals_pairs: Vec<(NaiveDate, f32)>,
    icu_activities: Vec<db::IntervalsActivity>,
    goals: Vec<db::AthleteGoal>,
    icu_workouts: Vec<db::IntervalsWorkout>,
    wellness: Vec<db::WellnessEntry>,
    time_off: Vec<db::TimeOffEntry>,
}

/// Load the history a workout suggestion is based on.
///
/// The first failure aborts: a partial read would still be sent, and the coach
/// would recommend a session having been shown a training history that is
/// missing rides — at the rider's expense, since the request is billed.
async fn load_suggestion_prompt_data(
    pool: &SqlitePool,
    today: NaiveDate,
) -> anyhow::Result<SuggestionPromptData> {
    let lookahead = today + CDuration::days(TIME_OFF_LOOKAHEAD_DAYS);
    Ok(SuggestionPromptData {
        athlete_ctx: db::get_setting(pool, "coaching.athlete_context")
            .await?
            .unwrap_or_default(),
        records: db::load_session_summaries(pool).await?,
        intervals_pairs: db::load_intervals_tss_pairs(pool).await?,
        icu_activities: db::load_unlinked_intervals_activities(pool).await?,
        goals: db::load_goals(pool).await?,
        icu_workouts: db::load_intervals_workouts(pool).await?,
        wellness: db::load_wellness_recent(pool, AI_WELLNESS_DAYS).await?,
        time_off: db::load_time_off_between(
            pool,
            &today.format("%Y-%m-%d").to_string(),
            &lookahead.format("%Y-%m-%d").to_string(),
        )
        .await?,
    })
}

/// The training history behind a multi-week program prompt.
struct ProgramPromptData {
    athlete_ctx: String,
    goals: Vec<db::AthleteGoal>,
    records: Vec<db::SessionSummary>,
    intervals_pairs: Vec<(NaiveDate, f32)>,
    icu_workouts: Vec<db::IntervalsWorkout>,
}

/// Load the history a training program is built from. Aborts on the first
/// failure, for the same reason as [`load_suggestion_prompt_data`].
async fn load_program_prompt_data(pool: &SqlitePool) -> anyhow::Result<ProgramPromptData> {
    Ok(ProgramPromptData {
        athlete_ctx: db::get_setting(pool, "coaching.athlete_context")
            .await?
            .unwrap_or_default(),
        goals: db::load_goals(pool).await?,
        records: db::load_session_summaries(pool).await?,
        intervals_pairs: db::load_intervals_tss_pairs(pool).await?,
        icu_workouts: db::load_intervals_workouts(pool).await?,
    })
}

pub struct CoachingPage {
    root: gtk::Box,
}

impl CoachingPage {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        athlete: Rc<RefCell<AthleteProfile>>,
        workouts: Vec<Workout>,
        on_start_workout: Rc<dyn Fn(Workout)>,
        toast_fn: Rc<dyn Fn(adw::Toast)>,
    ) -> (Self, Rc<dyn Fn()>) {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();

        // ── API key pre-flight banner ─────────────────────────────────────────
        let api_banner = adw::Banner::builder()
            .title("Add your AI provider API key in Preferences → Integrations to use AI features")
            .button_label("Open Preferences")
            .revealed(false)
            .build();
        // The banner only ever shows to someone with no key yet, so its button
        // has to take them somewhere. `app.preferences` is the same action the
        // main menu uses.
        api_banner.connect_button_clicked(|banner| {
            if let Err(e) = banner.activate_action("app.preferences", None) {
                tracing::error!("Could not open Preferences from the API key banner: {e}");
            }
        });
        root.append(&api_banner);

        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();

        let clamp = adw::Clamp::builder()
            .maximum_size(900)
            .margin_top(24)
            .margin_bottom(24)
            .margin_start(24)
            .margin_end(24)
            .build();

        let inner = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .build();

        let workouts = Rc::new(workouts);

        // ── Coach hero — ask for today's workout ──────────────────────────────
        // The page's one primary action. Training context and library sync
        // moved to Preferences (Athlete / Integrations) — the AI reads both
        // from the database at request time either way.
        let coach_header = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        coach_header.append(
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
        let suggestion_spinner = gtk::Spinner::new();
        suggestion_spinner.set_visible(false);
        coach_header.append(&suggestion_spinner);
        let get_btn = gtk::Button::builder()
            .label("Get Suggestion")
            .css_classes(["pill", "suggested-action"])
            .tooltip_text("Ask the AI Coach for a personalised workout suggestion")
            .valign(gtk::Align::Center)
            .build();
        coach_header.append(&get_btn);
        inner.append(&coach_header);

        let response_label = gtk::Label::builder()
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
        response_frame.append(&response_label);
        inner.append(&response_frame);

        // Suggested workout action card — the recommendation gets the workout-
        // profile treatment (WorkoutGraph thumbnail, like the library rows).
        let suggested_workout: Rc<RefCell<Option<Workout>>> = Rc::new(RefCell::new(None));

        let action_card_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(14)
            .margin_end(14)
            .build();

        let action_top_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .build();
        let thumb_holder = gtk::Box::builder().build();
        action_top_row.append(&thumb_holder);

        let action_text_col = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .valign(gtk::Align::Center)
            .build();
        let workout_title_label = gtk::Label::builder()
            .label("")
            .halign(gtk::Align::Start)
            .css_classes(["heading"])
            .build();
        action_text_col.append(&workout_title_label);
        let workout_detail_label = gtk::Label::builder()
            .label("")
            .halign(gtk::Align::Start)
            .css_classes(["dim-label"])
            .build();
        action_text_col.append(&workout_detail_label);
        action_top_row.append(&action_text_col);
        action_card_box.append(&action_top_row);

        let workout_btns_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .build();

        let load_now_btn = gtk::Button::builder()
            .label("Start")
            .css_classes(["pill"])
            .tooltip_text("Load this workout and start riding")
            .build();
        let schedule_one_btn = gtk::Button::builder()
            .label("Schedule")
            .css_classes(["pill"])
            .tooltip_text("Schedule this workout on the calendar")
            .build();

        workout_btns_row.append(&load_now_btn);
        workout_btns_row.append(&schedule_one_btn);
        action_card_box.append(&workout_btns_row);

        let workout_action_frame = gtk::Box::builder().css_classes(["card"]).build();
        workout_action_frame.append(&action_card_box);
        workout_action_frame.set_visible(false);
        inner.append(&workout_action_frame);

        // ── Training Goals ────────────────────────────────────────────────────
        let goals_header = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .build();
        goals_header.append(
            &gtk::Label::builder()
                .label("Goals")
                .halign(gtk::Align::Start)
                .hexpand(true)
                .css_classes(["heading"])
                .tooltip_text(
                    "Goals help the AI Coach give more targeted workout suggestions \
                     and training programs",
                )
                .build(),
        );
        let add_goal_btn = gtk::Button::builder()
            .label("Add Goal")
            .css_classes(["pill"])
            .tooltip_text("Add a training goal")
            .halign(gtk::Align::End)
            .build();
        goals_header.append(&add_goal_btn);
        inner.append(&goals_header);

        let goals_list = gtk::ListBox::builder()
            .css_classes(["boxed-list"])
            .selection_mode(gtk::SelectionMode::None)
            .build();
        inner.append(&goals_list);

        let no_goals_label = gtk::Label::builder()
            .label(
                "No training goals added yet. Goals help the AI Coach give more targeted \
                 workout suggestions and training programs.",
            )
            .css_classes(["dim-label"])
            .halign(gtk::Align::Center)
            .wrap(true)
            .visible(false)
            .build();
        inner.append(&no_goals_label);

        // ── Training Program ──────────────────────────────────────────────────
        let program_header = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        program_header.append(
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
        let build_spinner = gtk::Spinner::new();
        build_spinner.set_visible(false);
        program_header.append(&build_spinner);
        let build_btn = gtk::Button::builder()
            .label("Build Program")
            .css_classes(["pill"])
            .tooltip_text("Ask the AI Coach to generate a structured training program")
            .valign(gtk::Align::Center)
            .build();
        program_header.append(&build_btn);
        inner.append(&program_header);

        inner.append(
            &gtk::Label::builder()
                .label("Training days")
                .halign(gtk::Align::Start)
                .css_classes(["caption-heading", "dim-label"])
                .build(),
        );

        // Linked toggle group (calendar's Week|Month pattern, multi-select) —
        // native pressed state, no CSS hacks.
        let days_row = gtk::Box::builder().css_classes(["linked"]).build();

        const DAY_LABELS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
        const DAY_VALUES: [&str; 7] = [
            "monday",
            "tuesday",
            "wednesday",
            "thursday",
            "friday",
            "saturday",
            "sunday",
        ];

        let day_toggles: Vec<gtk::ToggleButton> = DAY_LABELS
            .iter()
            .map(|name| {
                let btn = gtk::ToggleButton::builder()
                    .label(*name)
                    .tooltip_text(format!("Train on {name}"))
                    .build();
                days_row.append(&btn);
                btn
            })
            .collect();

        // Default: Mon, Wed, Fri
        for i in [0usize, 2, 4] {
            day_toggles[i].set_active(true);
        }

        inner.append(&days_row);

        let months_adj = gtk::Adjustment::new(3.0, 1.0, 24.0, 1.0, 3.0, 0.0);
        let months_row = adw::SpinRow::new(Some(&months_adj), 1.0, 0);
        months_row.set_title("Duration (months)");
        months_row.set_tooltip_text(Some("Number of months for the training program"));

        let open_ended_row = adw::SwitchRow::builder()
            .title("Open-ended")
            .subtitle("Generate 8 weeks without a fixed end date")
            .tooltip_text("Build a program without a fixed end date")
            .build();

        let duration_list = gtk::ListBox::builder()
            .css_classes(["boxed-list"])
            .selection_mode(gtk::SelectionMode::None)
            .build();
        duration_list.append(&months_row);
        duration_list.append(&open_ended_row);
        inner.append(&duration_list);

        {
            let mr = months_row.clone();
            open_ended_row.connect_active_notify(move |row| {
                mr.set_sensitive(!row.is_active());
            });
        }

        let program_label = gtk::Label::builder()
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

        let program_frame = gtk::Box::builder()
            .css_classes(["card"])
            .orientation(gtk::Orientation::Vertical)
            .build();
        program_frame.append(&program_label);
        program_frame.set_visible(false);
        inner.append(&program_frame);

        let schedule_program_btn = gtk::Button::builder()
            .label("Schedule to Calendar")
            .css_classes(["pill"])
            .tooltip_text("Add all program workouts to your calendar starting from a chosen date")
            .halign(gtk::Align::Start)
            .visible(false)
            .build();
        inner.append(&schedule_program_btn);

        clamp.set_child(Some(&inner));
        scroll.set_child(Some(&clamp));
        root.append(&scroll);

        // ── Reload closure ────────────────────────────────────────────────────
        type ReloadHolder = Rc<RefCell<Option<Rc<dyn Fn()>>>>;
        let reload_holder: ReloadHolder = Rc::new(RefCell::new(None));

        let reload: Rc<dyn Fn()> = {
            let pool = pool.clone();
            let rt = rt_handle.clone();
            let goals_list = goals_list.clone();
            let no_goals_label = no_goals_label.clone();
            let pool_del = pool.clone();
            let rt_del = rt_handle.clone();
            let reload_holder = Rc::clone(&reload_holder);
            // For restoring cached AI suggestion
            let response_label_r = response_label.clone();
            let workout_action_frame_r = workout_action_frame.clone();
            let workout_title_label_r = workout_title_label.clone();
            let workout_detail_label_r = workout_detail_label.clone();
            let load_now_btn_r = load_now_btn.clone();
            let schedule_one_btn_r = schedule_one_btn.clone();
            let suggested_workout_r = Rc::clone(&suggested_workout);
            let workouts_r = Rc::clone(&workouts);
            let thumb_holder_r = thumb_holder.clone();
            let athlete_r = Rc::clone(&athlete);
            let api_banner_r = api_banner.clone();
            let on_toast_reload = Rc::clone(&toast_fn);

            Rc::new(move || {
                // API key pre-flight check (local keyring — fast, stays synchronous)
                let has_api_key = keystore::get_secret(keystore::KEY_ANTHROPIC)
                    .unwrap_or(None)
                    .map(|k| !k.trim().is_empty())
                    .unwrap_or(false);
                api_banner_r.set_revealed(!has_api_key);

                // Load coaching data off the main thread (CLAUDE.md §2.3); update the
                // profile, goal list, and cached suggestion once it arrives. Clone the
                // widget handles the callback needs.
                let pool_load = pool.clone();
                let goals_list = goals_list.clone();
                let no_goals_label = no_goals_label.clone();
                let pool_del = pool_del.clone();
                let rt_del = rt_del.clone();
                let reload_holder = Rc::clone(&reload_holder);
                let response_label_r = response_label_r.clone();
                let workout_action_frame_r = workout_action_frame_r.clone();
                let workout_title_label_r = workout_title_label_r.clone();
                let workout_detail_label_r = workout_detail_label_r.clone();
                let load_now_btn_r = load_now_btn_r.clone();
                let schedule_one_btn_r = schedule_one_btn_r.clone();
                let suggested_workout_r = Rc::clone(&suggested_workout_r);
                let workouts_r = Rc::clone(&workouts_r);
                let thumb_holder_r = thumb_holder_r.clone();
                let athlete_r = Rc::clone(&athlete_r);
                let on_toast_r = Rc::clone(&on_toast_reload);

                crate::ui::spawn_to_main(
                    &rt,
                    async move { load_coaching_data(&pool_load).await },
                    move |result| {
                        // A failed load must not redraw the page as empty: an
                        // empty goals list reads as "you have no goals", which is
                        // a statement about the rider, not about the database.
                        let CoachingData {
                            goals,
                            cached_resp,
                            cached_name,
                            cached_detail,
                        } = match result {
                            Ok(data) => data,
                            Err(e) => {
                                tracing::error!("Could not load coaching data: {e}");
                                on_toast_r(
                                    adw::Toast::builder()
                                        .title("Could not load your goals")
                                        .timeout(5)
                                        .build(),
                                );
                                return;
                            }
                        };

                        while let Some(row) = goals_list.row_at_index(0) {
                            goals_list.remove(&row);
                        }

                        if goals.is_empty() {
                            goals_list.set_visible(false);
                            no_goals_label.set_visible(true);
                        } else {
                            goals_list.set_visible(true);
                            no_goals_label.set_visible(false);

                            for goal in &goals {
                                let goal_id = goal.id;
                                let row =
                                    adw::ActionRow::builder().title(&goal.description).build();

                                let del_btn = gtk::Button::builder()
                                    .icon_name("user-trash-symbolic")
                                    .css_classes(["flat", "circular"])
                                    .tooltip_text("Remove this goal")
                                    .valign(gtk::Align::Center)
                                    .build();

                                let pool_d = pool_del.clone();
                                let rt_d = rt_del.clone();
                                let reload_d = Rc::clone(&reload_holder);
                                del_btn.connect_clicked(move |_| {
                                    let pool_d = pool_d.clone();
                                    let reload_d = Rc::clone(&reload_d);
                                    crate::ui::spawn_to_main(
                                        &rt_d,
                                        async move { db::delete_goal(&pool_d, goal_id).await },
                                        move |res| {
                                            if let Err(e) = res {
                                                tracing::error!("delete_goal failed: {e}");
                                            } else if let Some(f) = reload_d.borrow().as_ref() {
                                                f();
                                            }
                                        },
                                    );
                                });

                                row.add_suffix(&del_btn);
                                goals_list.append(&row);
                            }
                        }

                        // Restore cached AI suggestion if present
                        if !cached_resp.trim().is_empty() {
                            response_label_r.set_markup(&to_pango(&cached_resp));
                            response_label_r.remove_css_class("dim-label");

                            if !cached_name.is_empty() {
                                workout_title_label_r
                                    .set_label(&format!("Recommended: {}", cached_name));
                                workout_detail_label_r.set_label(&cached_detail);

                                // Restore action buttons only for built-in library workouts
                                let is_builtin = workouts_r.iter().find(|w| {
                                    crate::ai::naming::names_match(&w.name, &cached_name)
                                });
                                if let Some(w) = is_builtin {
                                    update_thumb(&thumb_holder_r, Some(w), &athlete_r.borrow());
                                    *suggested_workout_r.borrow_mut() = Some(w.clone());
                                    load_now_btn_r.set_visible(true);
                                    schedule_one_btn_r.set_visible(true);
                                } else {
                                    update_thumb(&thumb_holder_r, None, &athlete_r.borrow());
                                    *suggested_workout_r.borrow_mut() = None;
                                    load_now_btn_r.set_visible(false);
                                    schedule_one_btn_r.set_visible(false);
                                }
                                workout_action_frame_r.set_visible(true);
                            }
                        }
                    },
                );
            })
        };

        *reload_holder.borrow_mut() = Some(Rc::clone(&reload));

        // ── Add Goal ──────────────────────────────────────────────────────────
        {
            let pool_a = pool.clone();
            let rt_a = rt_handle.clone();
            let reload_a = Rc::clone(&reload_holder);

            add_goal_btn.connect_clicked(move |btn| {
                let dialog = adw::AlertDialog::new(Some("Add Training Goal"), None::<&str>);
                dialog.add_response("cancel", "Cancel");
                dialog.add_response("add", "Add");
                dialog.set_response_appearance("add", adw::ResponseAppearance::Suggested);
                dialog.set_default_response(Some("add"));
                dialog.set_close_response("cancel");

                let entry = gtk::Entry::builder()
                    .placeholder_text("e.g. Complete a 100 km sportive by September")
                    .hexpand(true)
                    .activates_default(true)
                    .build();
                dialog.set_extra_child(Some(&entry));

                let pool_d = pool_a.clone();
                let rt_d = rt_a.clone();
                let reload_d = Rc::clone(&reload_a);
                let entry_d = entry.clone();
                dialog.connect_response(None, move |_, response| {
                    if response == "add" {
                        let trimmed = entry_d.text().trim().to_string();
                        if !trimmed.is_empty() {
                            let pool_d = pool_d.clone();
                            let reload_d = Rc::clone(&reload_d);
                            crate::ui::spawn_to_main(
                                &rt_d,
                                async move { db::save_goal(&pool_d, &trimmed).await },
                                move |res| {
                                    if let Err(e) = res {
                                        tracing::error!("save_goal failed: {e}");
                                    } else if let Some(f) = reload_d.borrow().as_ref() {
                                        f();
                                    }
                                },
                            );
                        }
                    }
                });

                dialog.present(Some(btn));
                entry.grab_focus();
            });
        }

        // ── Get Suggestion ────────────────────────────────────────────────────
        {
            let pool_s = pool.clone();
            let rt_s = rt_handle.clone();
            let athlete_s = Rc::clone(&athlete);
            let workouts_s = Rc::clone(&workouts);
            let response_s = response_label.clone();
            let spinner_s = suggestion_spinner.clone();
            let suggested_s = Rc::clone(&suggested_workout);
            let action_frame_s = workout_action_frame.clone();
            let title_s = workout_title_label.clone();
            let detail_s = workout_detail_label.clone();
            let load_btn_s = load_now_btn.clone();
            let sched_btn_s = schedule_one_btn.clone();
            let thumb_s = thumb_holder.clone();

            get_btn.connect_clicked(move |btn| {
                let api_key = match keystore::get_secret(keystore::KEY_ANTHROPIC) {
                    Ok(Some(k)) if !k.trim().is_empty() => k,
                    _ => {
                        response_s.set_text(
                            "No AI provider key configured. Enter your API key in \
                                 Preferences → Integrations.",
                        );
                        response_s.remove_css_class("dim-label");
                        action_frame_s.set_visible(false);
                        return;
                    }
                };

                // Read !Send shared state on the main thread before spawning.
                let athlete = athlete_s.borrow().clone();
                let ftp = athlete.ftp_watts;
                let workouts_owned: Vec<Workout> = (*workouts_s).clone();

                btn.set_sensitive(false);
                spinner_s.set_visible(true);
                spinner_s.start();
                response_s.set_text("Asking the AI Coach for a suggestion…");
                response_s.remove_css_class("dim-label");
                action_frame_s.set_visible(false);
                *suggested_s.borrow_mut() = None;

                let (tx, rx) = async_channel::bounded::<
                    Result<(String, Vec<db::IntervalsWorkout>), AiFailure>,
                >(1);
                let pool_t = pool_s.clone();
                // All DB reads + prompt assembly + the network call run off the main
                // thread (CLAUDE.md §2.3). icu_workouts is returned to the result
                // handler so it can still match an Intervals.icu recommendation.
                rt_s.spawn(async move {
                    let today = Local::now().date_naive();
                    let SuggestionPromptData {
                        athlete_ctx,
                        records,
                        intervals_pairs,
                        icu_activities,
                        goals,
                        icu_workouts,
                        wellness: wellness_raw,
                        time_off,
                    } = match load_suggestion_prompt_data(&pool_t, today).await {
                        Ok(data) => data,
                        Err(e) => {
                            tracing::error!("Could not read training history to suggest: {e}");
                            let _ = tx.send(Err(AiFailure::DataUnavailable)).await;
                            return;
                        }
                    };

                    let m = compute_load_metrics(&records, &intervals_pairs, ftp, today);
                    let (ctl, atl, tsb) = (m.ctl, m.atl, m.tsb());

                    let since = today - CDuration::weeks(RECENT_TRAINING_WEEKS);
                    let mut recent: Vec<RecentSession> = records
                        .iter()
                        .filter(|r| r.started_at.with_timezone(&Local).date_naive() >= since)
                        .map(|r| build_recent_session(r, ftp))
                        .collect();

                    for act in icu_activities.iter().filter(|a| a.date >= since) {
                        recent.push(icu_activity_to_recent_session(act));
                    }
                    recent.sort_by(|a, b| b.date.cmp(&a.date));
                    recent.truncate(MAX_RECENT_SESSIONS);

                    let workout_opts = workouts_as_options(&workouts_owned, &icu_workouts);
                    let wellness = wellness_snapshots(&wellness_raw);
                    let time_off_dates: Vec<String> = time_off
                        .into_iter()
                        .map(|e| e.date.format("%Y-%m-%d").to_string())
                        .collect();

                    let ctx = TrainingContext {
                        athlete,
                        ctl,
                        atl,
                        tsb,
                        recent_sessions: recent,
                        goals,
                        athlete_context: athlete_ctx,
                        workout_options: workout_opts,
                        wellness,
                        time_off_dates,
                    };
                    let prompt = build_prompt(&ctx);

                    let result = get_suggestion(&api_key, &prompt, 1024)
                        .await
                        .map(|text| (text, icu_workouts))
                        .map_err(|e| {
                            tracing::error!("AI coaching request failed: {e}");
                            AiFailure::Request
                        });
                    let _ = tx.send(result).await;
                });

                let response_c = response_s.clone();
                let spinner_c = spinner_s.clone();
                let btn_c = btn.clone();
                let workouts_c = Rc::clone(&workouts_s);
                let suggested_c = Rc::clone(&suggested_s);
                let action_frame_c = action_frame_s.clone();
                let title_c = title_s.clone();
                let detail_c = detail_s.clone();
                let load_btn_c = load_btn_s.clone();
                let sched_btn_c = sched_btn_s.clone();
                let thumb_c = thumb_s.clone();
                let athlete_ui = Rc::clone(&athlete_s);
                let pool_cache = pool_s.clone();
                let rt_cache = rt_s.clone();

                glib::MainContext::default().spawn_local(async move {
                    if let Ok(result) = rx.recv().await {
                        match result {
                            Ok((text, icu_workouts_c)) => {
                                let recommended = extract_recommended_workout(&text);
                                let display_raw = strip_recommended_line(&text);
                                response_c.set_markup(&to_pango(&display_raw));
                                response_c.remove_css_class("dim-label");

                                let mut cache_name = recommended.clone().unwrap_or_default();
                                let mut cache_detail = String::new();

                                if let Some(ref rec_name) = recommended {
                                    // Check built-in library first
                                    if let Some(w) = workouts_c
                                        .iter()
                                        .find(|w| crate::ai::naming::names_match(&w.name, rec_name))
                                    {
                                        let detail = format!(
                                            "{} · {} min · TSS {:.0}",
                                            w.category.label(),
                                            w.duration_secs / 60,
                                            w.tss
                                        );
                                        cache_name = w.name.clone();
                                        cache_detail = detail.clone();
                                        title_c.set_label(&format!("Recommended: {}", w.name));
                                        detail_c.set_label(&detail);
                                        update_thumb(&thumb_c, Some(w), &athlete_ui.borrow());
                                        *suggested_c.borrow_mut() = Some(w.clone());
                                        load_btn_c.set_visible(true);
                                        sched_btn_c.set_visible(true);
                                        action_frame_c.set_visible(true);
                                    } else {
                                        // Check Intervals.icu library
                                        let lookup =
                                            rec_name.strip_prefix(ICU_PREFIX).unwrap_or(rec_name);
                                        if let Some(w) = icu_workouts_c.iter().find(|w| {
                                            crate::ai::naming::names_match(&w.name, lookup)
                                        }) {
                                            let dur = w
                                                .duration_secs
                                                .map(|s| format!("{} min", s / 60))
                                                .unwrap_or_else(|| "—".to_string());
                                            let tss_str = w
                                                .tss
                                                .map(|t| format!(" · TSS {:.0}", t))
                                                .unwrap_or_default();
                                            let detail = format!(
                                                "Intervals.icu · {dur}{tss_str} — open \
                                                 Intervals.icu to start this workout"
                                            );
                                            cache_name = format!("{ICU_PREFIX}{}", w.name);
                                            cache_detail = detail.clone();
                                            title_c.set_label(&format!(
                                                "Recommended: {} [Intervals.icu]",
                                                w.name
                                            ));
                                            detail_c.set_label(&detail);
                                            update_thumb(&thumb_c, None, &athlete_ui.borrow());
                                            *suggested_c.borrow_mut() = None;
                                            load_btn_c.set_visible(false);
                                            sched_btn_c.set_visible(false);
                                            action_frame_c.set_visible(true);
                                        }
                                    }
                                }

                                // Persist raw text (not markup) for dashboard + calendar restore
                                let pool_cc = pool_cache.clone();
                                rt_cache.spawn(async move {
                                    let _ = db::set_setting(
                                        &pool_cc,
                                        "ai.suggestion_response",
                                        &display_raw,
                                    )
                                    .await;
                                    let _ = db::set_setting(
                                        &pool_cc,
                                        "ai.suggestion_workout_name",
                                        &cache_name,
                                    )
                                    .await;
                                    let _ = db::set_setting(
                                        &pool_cc,
                                        "ai.suggestion_workout_detail",
                                        &cache_detail,
                                    )
                                    .await;
                                });
                            }
                            Err(failure) => response_c.set_text(failure.message()),
                        }
                    }
                    spinner_c.stop();
                    spinner_c.set_visible(false);
                    btn_c.set_sensitive(true);
                });
            });
        }

        // ── Load Now ──────────────────────────────────────────────────────────
        {
            let suggested_ln = Rc::clone(&suggested_workout);
            let on_start = Rc::clone(&on_start_workout);
            load_now_btn.connect_clicked(move |_| {
                if let Some(w) = suggested_ln.borrow().clone() {
                    on_start(w);
                }
            });
        }

        // ── Schedule single suggested workout ─────────────────────────────────
        {
            let pool_so = pool.clone();
            let rt_so = rt_handle.clone();
            let suggested_so = Rc::clone(&suggested_workout);
            let toast_so = Rc::clone(&toast_fn);

            schedule_one_btn.connect_clicked(move |btn| {
                let workout = match suggested_so.borrow().clone() {
                    Some(w) => w,
                    None => return,
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

                let today_str = Local::now().date_naive().format("%Y-%m-%d").to_string();
                let date_entry = adw::EntryRow::builder()
                    .title("Date (YYYY-MM-DD)")
                    .text(&today_str)
                    .input_hints(gtk::InputHints::NO_EMOJI)
                    .build();
                let date_list = gtk::ListBox::builder()
                    .css_classes(["boxed-list"])
                    .selection_mode(gtk::SelectionMode::None)
                    .build();
                date_list.append(&date_entry);
                dialog.set_extra_child(Some(&date_list));

                let pool_d = pool_so.clone();
                let rt_d = rt_so.clone();
                let toast_d = Rc::clone(&toast_so);
                let workout_id = workout.id;
                let workout_name = workout.name.clone();

                dialog.connect_response(None, move |_, response| {
                    if response == "schedule" {
                        let raw = date_entry.text().to_string();
                        if let Ok(date) = NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d") {
                            let date_str = date.format("%Y-%m-%d").to_string();
                            let pool_d = pool_d.clone();
                            let toast_d = Rc::clone(&toast_d);
                            let workout_name = workout_name.clone();
                            let date_for_msg = date_str.clone();
                            crate::ui::spawn_to_main(
                                &rt_d,
                                async move {
                                    db::schedule_workout(&pool_d, workout_id, &date_str).await
                                },
                                move |res| match res {
                                    Ok(_) => {
                                        toast_d(
                                            adw::Toast::builder()
                                                .title(format!(
                                                    "Scheduled {workout_name} on {date_for_msg}"
                                                ))
                                                .timeout(4)
                                                .build(),
                                        );
                                    }
                                    Err(e) => {
                                        tracing::error!("schedule_workout failed: {e}");
                                        toast_d(
                                            adw::Toast::builder()
                                                .title("Failed to schedule workout")
                                                .timeout(3)
                                                .build(),
                                        );
                                    }
                                },
                            );
                        } else {
                            toast_d(
                                adw::Toast::builder()
                                    .title("Invalid date — use YYYY-MM-DD format")
                                    .timeout(3)
                                    .build(),
                            );
                        }
                    }
                });

                dialog.present(Some(btn));
            });
        }

        // ── Build Program ─────────────────────────────────────────────────────
        let program_entries: Rc<RefCell<Vec<ProgramEntry>>> = Rc::new(RefCell::new(Vec::new()));

        {
            let pool_b = pool.clone();
            let rt_b = rt_handle.clone();
            let athlete_b = Rc::clone(&athlete);
            let workouts_b = Rc::clone(&workouts);
            let day_toggles_b = day_toggles.clone();
            let months_row_b = months_row.clone();
            let open_ended_row_b = open_ended_row.clone();
            let program_label_b = program_label.clone();
            let program_frame_b = program_frame.clone();
            let build_spinner_b = build_spinner.clone();
            let schedule_btn_b = schedule_program_btn.clone();
            let entries_b = Rc::clone(&program_entries);

            build_btn.connect_clicked(move |btn| {
                let api_key = match keystore::get_secret(keystore::KEY_ANTHROPIC) {
                    Ok(Some(k)) if !k.trim().is_empty() => k,
                    _ => {
                        program_frame_b.set_visible(true);
                        program_label_b.set_text(
                            "No AI provider key configured. Enter your API key in \
                                 Preferences → Integrations.",
                        );
                        return;
                    }
                };

                let training_days: Vec<String> = day_toggles_b
                    .iter()
                    .zip(DAY_VALUES.iter())
                    .filter(|(t, _)| t.is_active())
                    .map(|(_, d)| d.to_string())
                    .collect();

                if training_days.is_empty() {
                    program_frame_b.set_visible(true);
                    program_label_b.set_text("Please select at least one training day.");
                    return;
                }

                let num_weeks = if open_ended_row_b.is_active() {
                    None
                } else {
                    Some((months_row_b.value() as u32) * 4)
                };

                // Read !Send shared state on the main thread before spawning.
                let athlete = athlete_b.borrow().clone();
                let ftp = athlete.ftp_watts;
                let workouts_owned: Vec<Workout> = (*workouts_b).clone();

                btn.set_sensitive(false);
                build_spinner_b.set_visible(true);
                build_spinner_b.start();
                program_frame_b.set_visible(true);
                program_label_b.set_text("Building your training program…");
                schedule_btn_b.set_visible(false);
                *entries_b.borrow_mut() = Vec::new();

                let (tx, rx) = async_channel::bounded::<
                    Result<(String, Vec<db::IntervalsWorkout>), AiFailure>,
                >(1);
                let pool_t = pool_b.clone();
                // All DB reads + prompt assembly + the network call run off the main
                // thread (CLAUDE.md §2.3). icu_workouts is returned to the result
                // handler so it can format the program with Intervals.icu workouts.
                rt_b.spawn(async move {
                    let ProgramPromptData {
                        athlete_ctx,
                        goals,
                        records,
                        intervals_pairs,
                        icu_workouts,
                    } = match load_program_prompt_data(&pool_t).await {
                        Ok(data) => data,
                        Err(e) => {
                            tracing::error!("Could not read training history to plan: {e}");
                            let _ = tx.send(Err(AiFailure::DataUnavailable)).await;
                            return;
                        }
                    };

                    let today = Local::now().date_naive();
                    let m = compute_load_metrics(&records, &intervals_pairs, ftp, today);
                    let (ctl, tsb) = (m.ctl, m.tsb());

                    let workout_opts = workouts_as_options(&workouts_owned, &icu_workouts);

                    let ctx = ProgramContext {
                        athlete,
                        ctl,
                        tsb,
                        goals,
                        athlete_context: athlete_ctx,
                        workout_options: workout_opts,
                        training_days,
                        num_weeks,
                    };
                    let prompt = build_program_prompt(&ctx);

                    let result = get_suggestion(&api_key, &prompt, 2048)
                        .await
                        .map(|text| (text, icu_workouts))
                        .map_err(|e| {
                            tracing::error!("AI coaching request failed: {e}");
                            AiFailure::Request
                        });
                    let _ = tx.send(result).await;
                });

                let label_c = program_label_b.clone();
                let frame_c = program_frame_b.clone();
                let spinner_c = build_spinner_b.clone();
                let btn_c = btn.clone();
                let sched_btn_c = schedule_btn_b.clone();
                let entries_c = Rc::clone(&entries_b);
                let workouts_c = Rc::clone(&workouts_b);

                glib::MainContext::default().spawn_local(async move {
                    if let Ok(result) = rx.recv().await {
                        match result {
                            Ok((text, icu_workouts_c)) => {
                                let entries = parse_program_response(&text);
                                if entries.is_empty() {
                                    label_c.set_text(
                                        "Could not parse the program response. \
                                         Please try again.",
                                    );
                                } else {
                                    let display =
                                        format_program(&entries, &workouts_c, &icu_workouts_c);
                                    label_c.set_markup(&to_pango(&display));
                                    frame_c.set_visible(true);
                                    *entries_c.borrow_mut() = entries;
                                    sched_btn_c.set_visible(true);
                                }
                            }
                            Err(failure) => label_c.set_text(failure.message()),
                        }
                    }
                    spinner_c.stop();
                    spinner_c.set_visible(false);
                    btn_c.set_sensitive(true);
                });
            });
        }

        // ── Schedule Program to Calendar ──────────────────────────────────────
        {
            let pool_sp = pool.clone();
            let rt_sp = rt_handle.clone();
            let entries_sp = Rc::clone(&program_entries);
            let workouts_sp = Rc::clone(&workouts);
            let toast_sp = Rc::clone(&toast_fn);
            // Intervals.icu workouts are only displayed; they can't be scheduled (no segments).
            // Entries using them will be counted as skipped.

            schedule_program_btn.connect_clicked(move |btn| {
                let entries = entries_sp.borrow().clone();
                if entries.is_empty() {
                    return;
                }

                let dialog = adw::AlertDialog::new(
                    Some("Schedule Program"),
                    Some("Choose a start date. The program begins on the Monday of that week."),
                );
                dialog.add_response("cancel", "Cancel");
                dialog.add_response("schedule", "Schedule");
                dialog.set_response_appearance("schedule", adw::ResponseAppearance::Suggested);
                dialog.set_default_response(Some("schedule"));
                dialog.set_close_response("cancel");

                let today_str = Local::now().date_naive().format("%Y-%m-%d").to_string();
                let date_entry = adw::EntryRow::builder()
                    .title("Start date (YYYY-MM-DD)")
                    .text(&today_str)
                    .input_hints(gtk::InputHints::NO_EMOJI)
                    .build();
                let date_list = gtk::ListBox::builder()
                    .css_classes(["boxed-list"])
                    .selection_mode(gtk::SelectionMode::None)
                    .build();
                date_list.append(&date_entry);
                dialog.set_extra_child(Some(&date_list));

                let pool_d = pool_sp.clone();
                let rt_d = rt_sp.clone();
                let toast_d = Rc::clone(&toast_sp);
                let workouts_d = Rc::clone(&workouts_sp);
                let entries_d = entries;

                dialog.connect_response(None, move |_, response| {
                    if response == "schedule" {
                        let raw = date_entry.text().to_string();
                        let selected = NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d")
                            .unwrap_or_else(|_| Local::now().date_naive());
                        let days_back = selected.weekday().num_days_from_monday() as i64;
                        let start_monday = selected - CDuration::days(days_back);

                        // Resolve program entries → (workout id, date) on the main thread,
                        // since the library list is held behind a non-Send Rc. The DB writes
                        // then run on the tokio runtime over owned, Send data.
                        let mut to_schedule: Vec<(i64, String)> = Vec::new();
                        let mut skipped = 0u32;
                        for entry in &entries_d {
                            let day_offset = day_name_to_offset(&entry.day) as i64;
                            let week_offset = (entry.week as i64 - 1) * 7;
                            let entry_date =
                                start_monday + CDuration::days(week_offset + day_offset);
                            let date_str = entry_date.format("%Y-%m-%d").to_string();

                            if let Some(w) = workouts_d.iter().find(|w| {
                                crate::ai::naming::names_match(&w.name, &entry.workout_name)
                            }) {
                                to_schedule.push((w.id, date_str));
                            } else {
                                tracing::warn!(
                                    "Workout '{}' not in library — skipped",
                                    entry.workout_name
                                );
                                skipped += 1;
                            }
                        }

                        let pool_d = pool_d.clone();
                        let toast_d = Rc::clone(&toast_d);
                        crate::ui::spawn_to_main(
                            &rt_d,
                            async move {
                                let mut count = 0u32;
                                let mut errors = 0u32;
                                for (id, date_str) in to_schedule {
                                    match db::schedule_workout(&pool_d, id, &date_str).await {
                                        Ok(_) => count += 1,
                                        Err(e) => {
                                            tracing::error!(
                                                "schedule_workout {id} on {date_str}: {e}"
                                            );
                                            errors += 1;
                                        }
                                    }
                                }
                                (count, errors)
                            },
                            move |(count, errors)| {
                                let errors = errors + skipped;
                                let msg = if errors == 0 {
                                    format!("{count} workouts added to calendar")
                                } else {
                                    format!("{count} added, {errors} skipped")
                                };
                                toast_d(adw::Toast::builder().title(msg).timeout(5).build());
                            },
                        );
                    }
                });

                dialog.present(Some(btn));
            });
        }

        reload();

        (Self { root }, reload)
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Replace the suggestion card's thumbnail with `workout`'s profile drawing,
/// or hide the holder when the recommendation isn't in the built-in library.
fn update_thumb(holder: &gtk::Box, workout: Option<&Workout>, athlete: &AthleteProfile) {
    while let Some(child) = holder.first_child() {
        holder.remove(&child);
    }
    if let Some(w) = workout {
        let graph = WorkoutGraph::new(w, athlete.ftp_watts);
        graph.widget().set_content_width(120);
        graph.widget().set_content_height(56);
        graph.widget().set_valign(gtk::Align::Center);
        holder.append(graph.widget());
    }
    holder.set_visible(workout.is_some());
}
