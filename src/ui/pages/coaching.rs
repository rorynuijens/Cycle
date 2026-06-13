use adw::prelude::*;
use gtk::glib;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::ui::markdown::to_pango;

use chrono::{Datelike, Duration as CDuration, Local, NaiveDate};
use sqlx::SqlitePool;

use crate::ai::coach::{
    build_program_prompt, build_prompt, get_suggestion, parse_program_response, ProgramContext,
    ProgramEntry, RecentSession, TrainingContext, WellnessSnapshot, WorkoutOption,
};
use crate::data::{athlete::AthleteProfile, db, keystore, workout::Workout};

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

        // ── Athlete Profile ───────────────────────────────────────────────────
        let profile_header = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .build();
        profile_header.append(
            &gtk::Label::builder()
                .label("Athlete Profile")
                .halign(gtk::Align::Start)
                .hexpand(true)
                .css_classes(["heading"])
                .build(),
        );
        let edit_profile_btn = gtk::Button::builder()
            .label("Edit")
            .css_classes(["pill"])
            .tooltip_text("Edit your athlete profile and training context")
            .halign(gtk::Align::End)
            .build();
        profile_header.append(&edit_profile_btn);
        inner.append(&profile_header);

        inner.append(
            &gtk::Label::builder()
                .label(
                    "Tell the AI Coach about your age, lifestyle, training background, and time \
                     constraints. This context shapes every coaching response — the more detail \
                     you provide, the more personalised the advice.",
                )
                .css_classes(["dim-label"])
                .halign(gtk::Align::Start)
                .wrap(true)
                .build(),
        );

        let profile_label = gtk::Label::builder()
            .label("No profile set — select Edit to describe your training context.")
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

        let profile_frame = gtk::Box::builder()
            .css_classes(["card"])
            .orientation(gtk::Orientation::Vertical)
            .build();
        profile_frame.append(&profile_label);
        inner.append(&profile_frame);

        // ── Intervals.icu Library ─────────────────────────────────────────────
        let icu_header = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .build();
        icu_header.append(
            &gtk::Label::builder()
                .label("Intervals.icu Library")
                .halign(gtk::Align::Start)
                .hexpand(true)
                .css_classes(["heading"])
                .build(),
        );

        let sync_lib_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .halign(gtk::Align::End)
            .build();
        let sync_lib_spinner = gtk::Spinner::new();
        sync_lib_spinner.set_visible(false);
        let sync_lib_btn = gtk::Button::builder()
            .label("Sync Library")
            .css_classes(["pill"])
            .tooltip_text("Download your Intervals.icu workout library for AI suggestions")
            .halign(gtk::Align::End)
            .build();
        sync_lib_box.append(&sync_lib_spinner);
        sync_lib_box.append(&sync_lib_btn);
        icu_header.append(&sync_lib_box);
        inner.append(&icu_header);

        let icu_count_label = gtk::Label::builder()
            .label("No workouts synced yet. Sync your Intervals.icu library to include it in AI coaching suggestions.")
            .css_classes(["dim-label"])
            .halign(gtk::Align::Start)
            .wrap(true)
            .build();
        inner.append(&icu_count_label);

        // ── Training Goals ────────────────────────────────────────────────────
        let goals_header = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .build();
        goals_header.append(
            &gtk::Label::builder()
                .label("Training Goals")
                .halign(gtk::Align::Start)
                .hexpand(true)
                .css_classes(["heading"])
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

        // ── AI Coaching Suggestion ────────────────────────────────────────────
        inner.append(
            &gtk::Label::builder()
                .label("AI Coaching Suggestion")
                .halign(gtk::Align::Start)
                .css_classes(["heading"])
                .build(),
        );
        inner.append(
            &gtk::Label::builder()
                .label(
                    "The AI Coach analyses your training load, recent sessions, goals, and \
                     athlete profile to recommend a specific workout from your library.",
                )
                .css_classes(["dim-label"])
                .halign(gtk::Align::Start)
                .wrap(true)
                .build(),
        );

        let suggestion_action_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .halign(gtk::Align::Center)
            .build();

        let get_btn = gtk::Button::builder()
            .label("Get Suggestion")
            .css_classes(["pill"])
            .tooltip_text("Ask the AI Coach for a personalised workout suggestion")
            .build();

        let suggestion_spinner = gtk::Spinner::new();
        suggestion_spinner.set_visible(false);

        suggestion_action_row.append(&get_btn);
        suggestion_action_row.append(&suggestion_spinner);
        inner.append(&suggestion_action_row);

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

        // Suggested workout action card
        let suggested_workout: Rc<RefCell<Option<Workout>>> = Rc::new(RefCell::new(None));

        let action_card_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(14)
            .margin_end(14)
            .build();

        let workout_title_label = gtk::Label::builder()
            .label("")
            .halign(gtk::Align::Start)
            .css_classes(["heading"])
            .build();
        action_card_box.append(&workout_title_label);

        let workout_detail_label = gtk::Label::builder()
            .label("")
            .halign(gtk::Align::Start)
            .css_classes(["dim-label"])
            .build();
        action_card_box.append(&workout_detail_label);

        let workout_btns_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .build();

        let load_now_btn = gtk::Button::builder()
            .label("Load Now")
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

        // ── Training Program ──────────────────────────────────────────────────
        inner.append(
            &gtk::Label::builder()
                .label("Training Program")
                .halign(gtk::Align::Start)
                .css_classes(["heading"])
                .build(),
        );
        inner.append(
            &gtk::Label::builder()
                .label(
                    "Build a multi-week structured plan based on your goals and current fitness. \
                     The AI Coach applies progressive overload, selects workouts from your \
                     library, and includes recovery weeks. Choose your training days and \
                     program duration, then select Build Program.",
                )
                .css_classes(["dim-label"])
                .halign(gtk::Align::Start)
                .wrap(true)
                .build(),
        );

        inner.append(
            &gtk::Label::builder()
                .label("Select your available training days (select to toggle)")
                .halign(gtk::Align::Start)
                .css_classes(["caption-heading"])
                .build(),
        );

        let days_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();

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
                    .css_classes(["pill"])
                    .tooltip_text(format!("Train on {name}"))
                    .build();

                // Apply/remove accent fill as the toggle state changes
                btn.connect_toggled(|b| {
                    if b.is_active() {
                        b.add_css_class("suggested-action");
                    } else {
                        b.remove_css_class("suggested-action");
                    }
                });

                days_row.append(&btn);
                btn
            })
            .collect();

        // Default: Mon, Wed, Fri — apply initial active styling
        for i in [0usize, 2, 4] {
            day_toggles[i].set_active(true);
            day_toggles[i].add_css_class("suggested-action");
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

        let build_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .halign(gtk::Align::Center)
            .build();

        let build_btn = gtk::Button::builder()
            .label("Build Program")
            .css_classes(["pill"])
            .tooltip_text("Ask the AI Coach to generate a structured training program")
            .build();

        let build_spinner = gtk::Spinner::new();
        build_spinner.set_visible(false);

        build_row.append(&build_btn);
        build_row.append(&build_spinner);
        inner.append(&build_row);

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
            let profile_label = profile_label.clone();
            let icu_count_label = icu_count_label.clone();
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
            let api_banner_r = api_banner.clone();

            Rc::new(move || {
                // API key pre-flight check
                let has_api_key = keystore::get_secret(keystore::KEY_ANTHROPIC)
                    .unwrap_or(None)
                    .map(|k| !k.trim().is_empty())
                    .unwrap_or(false);
                api_banner_r.set_revealed(!has_api_key);

                let ctx = rt
                    .block_on(db::get_setting(&pool, "coaching.athlete_context"))
                    .unwrap_or(None)
                    .unwrap_or_default();

                // Intervals.icu workout count
                let icu_count = rt
                    .block_on(db::count_intervals_workouts(&pool))
                    .unwrap_or(0);
                if icu_count > 0 {
                    icu_count_label.set_text(&format!(
                        "{icu_count} workouts synced from Intervals.icu — included in AI suggestions."
                    ));
                    icu_count_label.remove_css_class("dim-label");
                } else {
                    icu_count_label.set_text(
                        "No workouts synced yet. Sync your Intervals.icu library to include \
                         it in AI coaching suggestions.",
                    );
                    icu_count_label.add_css_class("dim-label");
                }
                if ctx.trim().is_empty() {
                    profile_label
                        .set_text("No profile set — tap Edit to describe your training context.");
                    profile_label.add_css_class("dim-label");
                } else {
                    profile_label.set_text(&ctx);
                    profile_label.remove_css_class("dim-label");
                }

                let goals = rt.block_on(db::load_goals(&pool)).unwrap_or_default();

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
                        let row = adw::ActionRow::builder().title(&goal.description).build();

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
                let cached_resp = rt
                    .block_on(db::get_setting(&pool, "ai.suggestion_response"))
                    .unwrap_or(None)
                    .unwrap_or_default();
                if !cached_resp.trim().is_empty() {
                    response_label_r.set_markup(&to_pango(&cached_resp));
                    response_label_r.remove_css_class("dim-label");

                    let cached_name = rt
                        .block_on(db::get_setting(&pool, "ai.suggestion_workout_name"))
                        .unwrap_or(None)
                        .unwrap_or_default();
                    let cached_detail = rt
                        .block_on(db::get_setting(&pool, "ai.suggestion_workout_detail"))
                        .unwrap_or(None)
                        .unwrap_or_default();

                    if !cached_name.is_empty() {
                        workout_title_label_r.set_label(&format!("Recommended: {}", cached_name));
                        workout_detail_label_r.set_label(&cached_detail);

                        // Restore action buttons only for built-in library workouts
                        let is_builtin = workouts_r
                            .iter()
                            .find(|w| w.name.eq_ignore_ascii_case(&cached_name));
                        if let Some(w) = is_builtin {
                            *suggested_workout_r.borrow_mut() = Some(w.clone());
                            load_now_btn_r.set_visible(true);
                            schedule_one_btn_r.set_visible(true);
                        } else {
                            *suggested_workout_r.borrow_mut() = None;
                            load_now_btn_r.set_visible(false);
                            schedule_one_btn_r.set_visible(false);
                        }
                        workout_action_frame_r.set_visible(true);
                    }
                }
            })
        };

        *reload_holder.borrow_mut() = Some(Rc::clone(&reload));

        // ── Sync Intervals.icu Library ────────────────────────────────────────
        {
            let pool_sl = pool.clone();
            let rt_sl = rt_handle.clone();
            let spinner_sl = sync_lib_spinner.clone();
            let reload_sl = Rc::clone(&reload_holder);
            let toast_sl = Rc::clone(&toast_fn);

            sync_lib_btn.connect_clicked(move |btn| {
                let api_key = keystore::get_secret(keystore::KEY_INTERVALS_API)
                    .unwrap_or(None)
                    .unwrap_or_default();
                let athlete_id = rt_sl
                    .block_on(db::get_setting(&pool_sl, "intervals.athlete_id"))
                    .unwrap_or(None)
                    .unwrap_or_default();

                if api_key.trim().is_empty() || athlete_id.trim().is_empty() {
                    tracing::warn!(
                        "Intervals.icu credentials not configured — cannot sync library"
                    );
                    toast_sl(
                        adw::Toast::builder()
                            .title(
                                "Set your Intervals.icu API key and Athlete ID in \
                                 Preferences → Integrations",
                            )
                            .timeout(5)
                            .build(),
                    );
                    return;
                }

                btn.set_sensitive(false);
                spinner_sl.set_visible(true);
                spinner_sl.start();

                let pool_async = pool_sl.clone();
                let (tx, rx) = async_channel::bounded::<Result<usize, String>>(1);
                rt_sl.spawn(async move {
                    let result = match crate::ai::intervals::fetch_workouts(&athlete_id, &api_key)
                        .await
                    {
                        Ok(workouts) => {
                            match crate::data::db::clear_intervals_workouts(&pool_async).await {
                                Err(e) => Err(e.to_string()),
                                Ok(_) => {
                                    let count = workouts.len();
                                    for w in workouts {
                                        if let Err(e) = crate::data::db::upsert_intervals_workout(
                                            &pool_async,
                                            &w.id,
                                            &w.name,
                                            &w.description,
                                            w.target_duration,
                                            w.icu_training_load,
                                        )
                                        .await
                                        {
                                            tracing::error!("upsert intervals workout: {e}");
                                        }
                                    }
                                    Ok(count)
                                }
                            }
                        }
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = tx.send(result).await;
                });

                let btn_c = btn.clone();
                let spinner_c = spinner_sl.clone();
                let reload_c = Rc::clone(&reload_sl);
                glib::MainContext::default().spawn_local(async move {
                    if let Ok(result) = rx.recv().await {
                        match result {
                            Ok(_count) => {
                                if let Some(f) = reload_c.borrow().as_ref() {
                                    f();
                                }
                            }
                            Err(e) => {
                                tracing::error!("Intervals.icu library sync failed: {e}");
                            }
                        }
                    }
                    spinner_c.stop();
                    spinner_c.set_visible(false);
                    btn_c.set_sensitive(true);
                });
            });
        }

        // ── Edit Athlete Profile ──────────────────────────────────────────────
        {
            let pool_p = pool.clone();
            let rt_p = rt_handle.clone();
            let reload_p = Rc::clone(&reload_holder);
            let toast_p = Rc::clone(&toast_fn);

            edit_profile_btn.connect_clicked(move |btn| {
                let current = rt_p
                    .block_on(db::get_setting(&pool_p, "coaching.athlete_context"))
                    .unwrap_or(None)
                    .unwrap_or_default();

                let content_box = gtk::Box::builder()
                    .orientation(gtk::Orientation::Vertical)
                    .spacing(12)
                    .margin_top(12)
                    .margin_bottom(24)
                    .margin_start(24)
                    .margin_end(24)
                    .build();

                content_box.append(
                    &gtk::Label::builder()
                        .label(
                            "Describe your age, lifestyle, time constraints, and training \
                             preferences. The AI Coach uses this in every coaching response.",
                        )
                        .css_classes(["dim-label"])
                        .halign(gtk::Align::Start)
                        .wrap(true)
                        .build(),
                );

                let template_btn = gtk::Button::builder()
                    .label("Use template")
                    .css_classes(["pill"])
                    .tooltip_text("Fill in a starter template")
                    .halign(gtk::Align::Start)
                    .build();
                content_box.append(&template_btn);

                let text_view = gtk::TextView::builder()
                    .wrap_mode(gtk::WrapMode::Word)
                    .accepts_tab(false)
                    .hexpand(true)
                    .build();
                text_view.buffer().set_text(&current);

                let tv_scroll = gtk::ScrolledWindow::builder()
                    .hscrollbar_policy(gtk::PolicyType::Never)
                    .min_content_height(120)
                    .hexpand(true)
                    .build();
                tv_scroll.set_child(Some(&text_view));

                let tv_frame = gtk::Box::builder()
                    .css_classes(["card"])
                    .orientation(gtk::Orientation::Vertical)
                    .build();
                tv_frame.append(&tv_scroll);
                content_box.append(&tv_frame);

                {
                    let tv = text_view.clone();
                    template_btn.connect_clicked(move |_| {
                        tv.buffer().set_text(
                            "I am [AGE] years old [GENDER]. [DESCRIBE YOUR LIFESTYLE AND \
                             TIME CONSTRAINTS].\nMy training goals are: [LIST YOUR GOALS].\n\
                             I prefer workouts that are [PREFERENCES — e.g. time-efficient, \
                             varied, low-impact].\nAdditional notes: [ANYTHING ELSE].",
                        );
                    });
                }

                let toolbar_view = adw::ToolbarView::new();
                let header = adw::HeaderBar::new();

                let cancel_btn = gtk::Button::builder()
                    .label("Cancel")
                    .tooltip_text("Discard changes")
                    .build();
                let save_btn = gtk::Button::builder()
                    .label("Save")
                    .css_classes(["suggested-action"])
                    .tooltip_text("Save athlete profile")
                    .build();
                header.pack_start(&cancel_btn);
                header.pack_end(&save_btn);
                toolbar_view.add_top_bar(&header);
                toolbar_view.set_content(Some(&content_box));

                let dialog = adw::Dialog::builder()
                    .title("Athlete Profile")
                    .child(&toolbar_view)
                    .content_width(560)
                    .build();

                let dialog_cancel = dialog.clone();
                cancel_btn.connect_clicked(move |_| {
                    dialog_cancel.close();
                });

                let pool_d = pool_p.clone();
                let rt_d = rt_p.clone();
                let reload_d = Rc::clone(&reload_p);
                let toast_d = Rc::clone(&toast_p);
                let dialog_save = dialog.clone();
                save_btn.connect_clicked(move |_| {
                    let buf = text_view.buffer();
                    let text = buf
                        .text(&buf.start_iter(), &buf.end_iter(), false)
                        .to_string();
                    let trimmed = text.trim().to_string();
                    let pool = pool_d.clone();
                    rt_d.spawn(async move {
                        if let Err(e) =
                            db::set_setting(&pool, "coaching.athlete_context", &trimmed).await
                        {
                            tracing::error!("save coaching.athlete_context failed: {e}");
                        }
                    });
                    toast_d(
                        adw::Toast::builder()
                            .title("Profile saved")
                            .timeout(3)
                            .build(),
                    );
                    if let Some(f) = reload_d.borrow().as_ref() {
                        f();
                    }
                    dialog_save.close();
                });

                dialog.present(Some(btn));
            });
        }

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

                let athlete_ctx = rt_s
                    .block_on(db::get_setting(&pool_s, "coaching.athlete_context"))
                    .unwrap_or(None)
                    .unwrap_or_default();

                let records = rt_s
                    .block_on(db::load_session_records(&pool_s))
                    .unwrap_or_default();
                let intervals_pairs = rt_s
                    .block_on(db::load_intervals_tss_pairs(&pool_s))
                    .unwrap_or_default();
                let icu_activities = rt_s
                    .block_on(db::load_intervals_activities(&pool_s))
                    .unwrap_or_default();
                let goals = rt_s.block_on(db::load_goals(&pool_s)).unwrap_or_default();
                let icu_workouts = rt_s
                    .block_on(db::load_intervals_workouts(&pool_s))
                    .unwrap_or_default();
                let wellness_raw = rt_s
                    .block_on(db::load_wellness_recent(&pool_s, 7))
                    .unwrap_or_default();
                let athlete = athlete_s.borrow().clone();
                let ftp = athlete.ftp_watts;

                let today = Local::now().date_naive();
                let (ctl, atl) = compute_ctl_atl(&records, &intervals_pairs, ftp, today);
                let tsb = ctl - atl;

                let four_weeks_ago = today - CDuration::weeks(4);
                let mut recent: Vec<RecentSession> = records
                    .iter()
                    .filter(|r| {
                        r.session.started_at.with_timezone(&Local).date_naive() >= four_weeks_ago
                    })
                    .map(|r| build_recent_session(r, ftp))
                    .collect();

                for act in icu_activities.iter().filter(|a| a.date >= four_weeks_ago) {
                    recent.push(icu_activity_to_recent_session(act));
                }
                recent.sort_by(|a, b| b.date.cmp(&a.date));
                recent.truncate(10);

                let workout_opts = workouts_as_options(&workouts_s, &icu_workouts);

                let wellness: Vec<WellnessSnapshot> = wellness_raw
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
                    .collect();

                let two_weeks_out = today + CDuration::days(14);
                let start_str = today.format("%Y-%m-%d").to_string();
                let end_str = two_weeks_out.format("%Y-%m-%d").to_string();
                let time_off_dates: Vec<String> = rt_s
                    .block_on(db::load_time_off_between(&pool_s, &start_str, &end_str))
                    .unwrap_or_default()
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

                btn.set_sensitive(false);
                spinner_s.set_visible(true);
                spinner_s.start();
                response_s.set_text("Asking the AI Coach for a suggestion…");
                response_s.remove_css_class("dim-label");
                action_frame_s.set_visible(false);
                *suggested_s.borrow_mut() = None;

                let (tx, rx) = async_channel::bounded::<Result<String, String>>(1);
                rt_s.spawn(async move {
                    let result = get_suggestion(&api_key, &prompt, 1024)
                        .await
                        .map_err(|e| e.to_string());
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
                let icu_workouts_c = icu_workouts;
                let pool_cache = pool_s.clone();
                let rt_cache = rt_s.clone();

                glib::MainContext::default().spawn_local(async move {
                    if let Ok(result) = rx.recv().await {
                        match result {
                            Ok(text) => {
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
                                        .find(|w| w.name.eq_ignore_ascii_case(rec_name))
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
                                        *suggested_c.borrow_mut() = Some(w.clone());
                                        load_btn_c.set_visible(true);
                                        sched_btn_c.set_visible(true);
                                        action_frame_c.set_visible(true);
                                    } else {
                                        // Check Intervals.icu library
                                        let lookup = rec_name
                                            .strip_prefix("[Intervals.icu] ")
                                            .unwrap_or(rec_name);
                                        if let Some(w) = icu_workouts_c
                                            .iter()
                                            .find(|w| w.name.eq_ignore_ascii_case(lookup))
                                        {
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
                                            cache_name = format!("[Intervals.icu] {}", w.name);
                                            cache_detail = detail.clone();
                                            title_c.set_label(&format!(
                                                "Recommended: {} [Intervals.icu]",
                                                w.name
                                            ));
                                            detail_c.set_label(&detail);
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
                            Err(e) => {
                                tracing::error!("AI suggestion request failed: {e}");
                                response_c.set_text(
                                    "The AI Coach couldn't complete this request. \
                                     Please check your API key and try again.",
                                );
                            }
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

                let athlete_ctx = rt_b
                    .block_on(db::get_setting(&pool_b, "coaching.athlete_context"))
                    .unwrap_or(None)
                    .unwrap_or_default();

                let goals = rt_b.block_on(db::load_goals(&pool_b)).unwrap_or_default();
                let records = rt_b
                    .block_on(db::load_session_records(&pool_b))
                    .unwrap_or_default();
                let intervals_pairs = rt_b
                    .block_on(db::load_intervals_tss_pairs(&pool_b))
                    .unwrap_or_default();
                let icu_workouts = rt_b
                    .block_on(db::load_intervals_workouts(&pool_b))
                    .unwrap_or_default();
                let athlete = athlete_b.borrow().clone();
                let ftp = athlete.ftp_watts;
                let today = Local::now().date_naive();

                let (ctl, atl) = compute_ctl_atl(&records, &intervals_pairs, ftp, today);
                let tsb = ctl - atl;

                let workout_opts = workouts_as_options(&workouts_b, &icu_workouts);

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

                btn.set_sensitive(false);
                build_spinner_b.set_visible(true);
                build_spinner_b.start();
                program_frame_b.set_visible(true);
                program_label_b.set_text("Building your training program…");
                schedule_btn_b.set_visible(false);
                *entries_b.borrow_mut() = Vec::new();

                let (tx, rx) = async_channel::bounded::<Result<String, String>>(1);
                rt_b.spawn(async move {
                    let result = get_suggestion(&api_key, &prompt, 2048)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx.send(result).await;
                });

                let label_c = program_label_b.clone();
                let frame_c = program_frame_b.clone();
                let spinner_c = build_spinner_b.clone();
                let btn_c = btn.clone();
                let sched_btn_c = schedule_btn_b.clone();
                let entries_c = Rc::clone(&entries_b);
                let workouts_c = Rc::clone(&workouts_b);
                let icu_workouts_c = icu_workouts;

                glib::MainContext::default().spawn_local(async move {
                    if let Ok(result) = rx.recv().await {
                        match result {
                            Ok(text) => {
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
                            Err(e) => {
                                tracing::error!("AI program build failed: {e}");
                                label_c.set_text(
                                    "The AI Coach couldn't complete this request. \
                                     Please check your API key and try again.",
                                );
                            }
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

                            if let Some(w) = workouts_d
                                .iter()
                                .find(|w| w.name.eq_ignore_ascii_case(&entry.workout_name))
                            {
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

fn compute_ctl_atl(
    records: &[db::SessionRecord],
    intervals_pairs: &[(NaiveDate, f32)],
    ftp: u32,
    today: NaiveDate,
) -> (f64, f64) {
    let mut daily_tss: HashMap<NaiveDate, f32> = HashMap::new();
    for r in records {
        if r.uploaded_to_icu {
            continue;
        }
        let date = r.session.started_at.with_timezone(&Local).date_naive();
        if let Some(tss) = r.session.tss(ftp) {
            *daily_tss.entry(date).or_insert(0.0) += tss;
        }
    }
    for &(date, tss) in intervals_pairs {
        *daily_tss.entry(date).or_insert(0.0) += tss;
    }
    let Some(earliest) = daily_tss.keys().min().copied() else {
        return (0.0, 0.0);
    };
    let ctl_alpha = 1.0_f64 - (-1.0_f64 / 42.0).exp();
    let atl_alpha = 1.0_f64 - (-1.0_f64 / 7.0).exp();
    let mut ctl = 0.0_f64;
    let mut atl = 0.0_f64;
    let mut date = earliest;
    loop {
        let tss = daily_tss.get(&date).copied().unwrap_or(0.0) as f64;
        ctl += ctl_alpha * (tss - ctl);
        atl += atl_alpha * (tss - atl);
        if date == today {
            break;
        }
        match date.succ_opt() {
            Some(next) => date = next,
            None => break,
        }
    }
    (ctl, atl)
}

fn build_recent_session(r: &db::SessionRecord, ftp: u32) -> RecentSession {
    let dur_secs = r.session.duration_secs();
    let power_readings: Vec<u32> = r
        .session
        .data_points
        .iter()
        .filter_map(|dp| dp.power_watts)
        .collect();
    let avg_power = if power_readings.is_empty() {
        None
    } else {
        let sum: u64 = power_readings.iter().map(|&p| p as u64).sum();
        Some((sum / power_readings.len() as u64) as u32)
    };
    RecentSession {
        date: r
            .session
            .started_at
            .with_timezone(&Local)
            .format("%Y-%m-%d")
            .to_string(),
        name: r.workout_name.clone(),
        sport_type: "Cycling".to_string(),
        duration_mins: (dur_secs / 60) as u32,
        avg_power,
        tss: r.session.tss(ftp),
        kj: r.session.kilojoules(),
        rpe: r.session.rpe,
    }
}

fn icu_activity_to_recent_session(a: &db::IntervalsActivity) -> RecentSession {
    let sport = normalize_sport_type(&a.sport_type);
    // Use activity name when available; fall back to sport type rather than nothing
    let activity_name = if a.name.is_empty() {
        None
    } else {
        Some(a.name.clone())
    };
    let is_cycling = sport == "Cycling";
    RecentSession {
        date: a.date.format("%Y-%m-%d").to_string(),
        name: activity_name,
        sport_type: sport,
        duration_mins: a.duration_secs.map(|s| s / 60).unwrap_or(0),
        // Only pass power for cycling — running power (Stryd etc.) uses different units
        avg_power: if is_cycling { a.average_watts } else { None },
        tss: a.tss,
        kj: if is_cycling {
            a.average_watts
                .and_then(|w| a.duration_secs.map(|d| w as f32 * d as f32 / 1000.0))
                .unwrap_or(0.0)
        } else {
            0.0
        },
        rpe: None,
    }
}

/// Map the raw sport_type string from Intervals.icu / Garmin / Strava to a clean label.
pub(crate) fn normalize_sport_type(raw: &str) -> String {
    match raw {
        "" | "Ride" | "VirtualRide" | "Cycling" | "IndoorCycling" | "MountainBiking" => "Cycling",
        "Run" | "VirtualRun" | "TrailRun" => "Run",
        "Walk" | "Walking" => "Walk",
        "Hike" | "Hiking" => "Hike",
        "Swim" | "Swimming" | "OpenWaterSwim" => "Swim",
        "WeightTraining" | "Strength" | "StrengthTraining" => "Strength Training",
        "Yoga" => "Yoga",
        "Rowing" | "IndoorRowing" => "Rowing",
        "Elliptical" => "Elliptical",
        "NordicSki" | "BackcountrySki" => "Ski",
        "Workout" | "Crossfit" | "HIIT" => "Cross Training",
        other => other,
    }
    .to_string()
}

fn workouts_as_options(
    workouts: &[Workout],
    icu_workouts: &[db::IntervalsWorkout],
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
            name: format!("[Intervals.icu] {}", w.name),
            duration_mins: w.duration_secs.map(|s| s / 60).unwrap_or(60),
            tss: w.tss.unwrap_or(0.0),
            category: "Intervals.icu".to_string(),
        });
    }
    opts
}

fn extract_recommended_workout(text: &str) -> Option<String> {
    text.lines()
        .find(|l| l.trim_start().starts_with("RECOMMENDED_WORKOUT:"))
        .map(|l| {
            l.trim_start()
                .trim_start_matches("RECOMMENDED_WORKOUT:")
                .trim()
                .to_string()
        })
        .filter(|s| !s.is_empty())
}

fn strip_recommended_line(text: &str) -> String {
    text.lines()
        .filter(|l| !l.trim_start().starts_with("RECOMMENDED_WORKOUT:"))
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_string()
}

fn format_program(
    entries: &[ProgramEntry],
    workouts: &[Workout],
    icu_workouts: &[db::IntervalsWorkout],
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
            .find(|w| w.name.eq_ignore_ascii_case(&entry.workout_name))
            .map(|w| format!(" ({} min)", w.duration_secs / 60))
            .or_else(|| {
                let lookup = entry
                    .workout_name
                    .strip_prefix("[Intervals.icu] ")
                    .unwrap_or(&entry.workout_name);
                icu_workouts
                    .iter()
                    .find(|w| w.name.eq_ignore_ascii_case(lookup))
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

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

fn day_name_to_offset(day: &str) -> u32 {
    match day.to_lowercase().as_str() {
        "monday" => 0,
        "tuesday" => 1,
        "wednesday" => 2,
        "thursday" => 3,
        "friday" => 4,
        "saturday" => 5,
        "sunday" => 6,
        _ => 0,
    }
}
