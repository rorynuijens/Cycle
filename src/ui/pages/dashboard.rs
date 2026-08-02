use adw::prelude::*;
use async_channel;
use chrono::{Duration, Local, NaiveDate, Timelike};
use gtk::glib;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use sqlx::SqlitePool;

use crate::ai::briefing::{
    build_briefing_prompt, parse_alternative_workout, parse_briefing_decision, BriefingContext,
    BriefingDecision, PlannedWorkout,
};
use crate::ai::coach::{get_suggestion, WellnessSnapshot, WorkoutOption};
use crate::data::{
    athlete::AthleteProfile,
    db::{self},
    keystore,
    workout::Workout,
};
use crate::ui::markdown::{strip_markdown, to_pango};
use crate::ui::widgets::workout_graph::WorkoutGraph;

type ReloadHolder = Rc<RefCell<Option<Rc<dyn Fn()>>>>;

pub struct DashboardPage {
    root: gtk::Box,
}

impl DashboardPage {
    /// Returns `(page, reload_fn)`. Call `reload_fn()` whenever data may have changed.
    pub fn new(
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        athlete: AthleteProfile,
        on_start: Rc<dyn Fn(Workout)>,
        on_view_fitness: Rc<dyn Fn()>,
        on_open_calendar: Rc<dyn Fn()>,
    ) -> (Self, Rc<dyn Fn()>) {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();

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

        // ── Header: greeting and date share one baseline ─────────────────────
        let header_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .build();

        let greeting_label = gtk::Label::builder()
            .label("")
            .halign(gtk::Align::Start)
            .hexpand(true)
            .valign(gtk::Align::Baseline)
            .css_classes(["title-1"])
            .build();

        let subtitle_label = gtk::Label::builder()
            .label("")
            .halign(gtk::Align::End)
            .valign(gtk::Align::Baseline)
            .css_classes(["dim-label"])
            .build();

        header_row.append(&greeting_label);
        header_row.append(&subtitle_label);
        inner.append(&header_row);

        // ── Today hero (rebuilt on each reload) ──────────────────────────────
        // The page's job is "what do I ride today?" — so today's workout,
        // drawn in its zone colours, comes before everything else.
        let dynamic_top = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .build();
        inner.append(&dynamic_top);

        // ── Coach briefing card (static — not rebuilt on each reload) ────────
        let reload_holder: ReloadHolder = Rc::new(RefCell::new(None));
        let briefing_card = Self::build_briefing_card(
            pool.clone(),
            rt_handle.clone(),
            athlete.clone(),
            Rc::clone(&reload_holder),
        );
        inner.append(&briefing_card);

        // ── Form + recent activity (rebuilt on each reload) ──────────────────
        let dynamic = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .build();
        inner.append(&dynamic);

        clamp.set_child(Some(&inner));
        scroll.set_child(Some(&clamp));
        root.append(&scroll);

        let reload: Rc<dyn Fn()> = {
            let greeting_label = greeting_label.clone();
            let subtitle_label = subtitle_label.clone();
            let dynamic_top = dynamic_top.clone();
            let dynamic = dynamic.clone();
            let briefing_card = briefing_card.clone();
            let pool = pool.clone();
            let rt_handle = rt_handle.clone();
            let athlete = athlete.clone();
            let on_start = Rc::clone(&on_start);
            let on_view_fitness = Rc::clone(&on_view_fitness);
            let on_open_calendar = Rc::clone(&on_open_calendar);
            let reload_holder = Rc::clone(&reload_holder);

            Rc::new(move || {
                let now = Local::now();
                let ftp = athlete.ftp_watts;

                let salutation = match now.hour() {
                    5..=11 => "Good morning",
                    12..=17 => "Good afternoon",
                    _ => "Good evening",
                };
                greeting_label.set_label(&format!("{}, {}", salutation, athlete.name));

                let today_naive = now.date_naive();
                let weekday = today_naive.format("%A, %-d %B %Y").to_string();
                subtitle_label.set_label(&weekday);

                let today_str = today_naive.format("%Y-%m-%d").to_string();

                // Load everything the summary cards need off the main thread
                // (CLAUDE.md §2.3), then rebuild the dynamic areas on arrival.
                let pool_load = pool.clone();
                let dynamic_top = dynamic_top.clone();
                let dynamic = dynamic.clone();
                let briefing_card = briefing_card.clone();
                let athlete_cb = athlete.clone();
                let on_start = Rc::clone(&on_start);
                let on_view_fitness = Rc::clone(&on_view_fitness);
                let on_open_calendar = Rc::clone(&on_open_calendar);
                let reload_holder = Rc::clone(&reload_holder);
                let today_for_card = today_str.clone();
                let pool_ob = pool.clone();
                let rt_ob = rt_handle.clone();

                crate::ui::spawn_to_main(
                    &rt_handle,
                    async move {
                        let today_entry = db::load_today_entry(&pool_load, &today_str)
                            .await
                            .unwrap_or(None);
                        let records = db::load_session_records(&pool_load)
                            .await
                            .unwrap_or_default();
                        let ai_workout_name =
                            db::get_setting(&pool_load, "ai.suggestion_workout_name")
                                .await
                                .unwrap_or(None)
                                .unwrap_or_default();
                        // The suggestion only matters on an empty day — with a
                        // workout scheduled, the plan wins and the suggestion hides.
                        let suggested_workout =
                            if today_entry.is_none() && !ai_workout_name.trim().is_empty() {
                                db::load_workouts(&pool_load)
                                    .await
                                    .unwrap_or_default()
                                    .into_iter()
                                    // Case-insensitive — the AI may return different casing
                                    .find(|w| w.name.eq_ignore_ascii_case(ai_workout_name.trim()))
                            } else {
                                None
                            };
                        let ai_workout_detail =
                            db::get_setting(&pool_load, "ai.suggestion_workout_detail")
                                .await
                                .unwrap_or(None)
                                .unwrap_or_default();
                        let ai_fitness_insight = db::get_setting(&pool_load, "ai.fitness_insight")
                            .await
                            .unwrap_or(None)
                            .unwrap_or_default();
                        let intervals_pairs = db::load_intervals_tss_pairs(&pool_load)
                            .await
                            .unwrap_or_default();
                        let first_use_done = db::get_setting(&pool_load, "first_use_complete")
                            .await
                            .unwrap_or(None)
                            .map(|v| v == "1")
                            .unwrap_or(false);
                        (
                            today_entry,
                            records,
                            suggested_workout,
                            ai_workout_detail,
                            ai_fitness_insight,
                            intervals_pairs,
                            first_use_done,
                        )
                    },
                    move |(
                        today_entry,
                        records,
                        suggested_workout,
                        ai_workout_detail,
                        ai_fitness_insight,
                        intervals_pairs,
                        first_use_done,
                    )| {
                        // Compute TSB for readiness + 7-day trend
                        let (ctl, atl) =
                            compute_ctl_atl(&records, &intervals_pairs, ftp, today_naive);
                        let tsb = ctl - atl;
                        let week_ago = today_naive - Duration::days(7);
                        let (ctl_7d, atl_7d) =
                            compute_ctl_atl(&records, &intervals_pairs, ftp, week_ago);

                        while let Some(child) = dynamic_top.first_child() {
                            dynamic_top.remove(&child);
                        }
                        while let Some(child) = dynamic.first_child() {
                            dynamic.remove(&child);
                        }

                        // ── First-run onboarding ─────────────────────────────
                        if !first_use_done
                            && records.is_empty()
                            && today_entry.is_none()
                            && ctl == 0.0
                        {
                            briefing_card.set_visible(false);
                            let get_started_btn = gtk::Button::builder()
                                .label("Get Started")
                                .css_classes(["pill", "suggested-action"])
                                .tooltip_text("Run the first-use setup wizard")
                                .build();

                            let status = adw::StatusPage::builder()
                                .icon_name("media-playback-start-symbolic")
                                .title("Welcome to Cycle")
                                .description(
                                    "Set up your profile and connect your services to get \
                                     personalised training recommendations.",
                                )
                                .child(&get_started_btn)
                                .build();

                            let dynamic_ob = dynamic_top.clone();
                            get_started_btn.connect_clicked(move |btn| {
                                let root = btn.root().and_downcast::<gtk::Window>();
                                let pool_w = pool_ob.clone();
                                let rt_w = rt_ob.clone();
                                let dynamic_w = dynamic_ob.clone();
                                super::onboarding::show(
                                    root.as_ref(),
                                    pool_w,
                                    rt_w,
                                    Rc::new(move || {
                                        // After wizard, remove the status page so the
                                        // dashboard reloads to the normal view next visit.
                                        while let Some(child) = dynamic_w.first_child() {
                                            dynamic_w.remove(&child);
                                        }
                                    }),
                                );
                            });

                            dynamic_top.append(&status);
                            return;
                        }
                        briefing_card.set_visible(true);

                        // ── Today hero: the workout you're about to ride ──────
                        dynamic_top.append(&Self::build_workout_card(
                            today_entry,
                            suggested_workout,
                            &ai_workout_detail,
                            Rc::clone(&on_start),
                            &athlete_cb,
                            Rc::clone(&on_open_calendar),
                            pool_ob.clone(),
                            rt_ob.clone(),
                            Rc::clone(&reload_holder),
                            today_for_card.clone(),
                        ));

                        // ── Form + last activity list ─────────────────────────
                        dynamic.append(&Self::build_status_list(
                            ctl,
                            atl,
                            tsb,
                            ctl_7d,
                            atl_7d,
                            &ai_fitness_insight,
                            records.first(),
                            ftp,
                            Rc::clone(&on_view_fitness),
                            Rc::clone(&on_open_calendar),
                        ));
                    },
                );
            })
        };

        *reload_holder.borrow_mut() = Some(Rc::clone(&reload));
        reload();

        (Self { root }, reload)
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    fn build_briefing_card(
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        athlete: AthleteProfile,
        reload_holder: ReloadHolder,
    ) -> adw::PreferencesGroup {
        let today_str = Local::now().date_naive().format("%Y-%m-%d").to_string();

        let group = adw::PreferencesGroup::builder().title("Coach").build();

        // ── Briefing text label ───────────────────────────────────────────────
        let text_label = gtk::Label::builder()
            .label("")
            .halign(gtk::Align::Start)
            .wrap(true)
            .xalign(0.0)
            .selectable(true)
            .css_classes(["body"])
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(12)
            .margin_end(12)
            .visible(false)
            .build();

        // ── Decision badge row ─────────────────────────────────────────────────
        let action_row = adw::ActionRow::builder().title("").visible(false).build();

        let decision_badge = gtk::Label::builder()
            .label("")
            .css_classes(["pill", "caption"])
            .valign(gtk::Align::Center)
            .build();
        action_row.add_suffix(&decision_badge);

        // Modify: "Replace" button (hidden unless decision is Modify)
        let use_workout_btn = gtk::Button::builder()
            .label("Replace")
            .css_classes(["pill"])
            .valign(gtk::Align::Center)
            .visible(false)
            .tooltip_text("Replace today's planned workout with the suggested workout")
            .build();
        action_row.add_suffix(&use_workout_btn);

        // Rest: "Remove from calendar" button (hidden unless decision is Rest)
        let remove_btn = gtk::Button::builder()
            .label("Remove from calendar")
            .css_classes(["pill", "destructive-action"])
            .valign(gtk::Align::Center)
            .visible(false)
            .tooltip_text("Remove today's workout — take a rest day as recommended")
            .build();
        action_row.add_suffix(&remove_btn);

        // ── Text label as a fake row (inlined via Box) ────────────────────────
        let text_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .margin_start(12)
            .margin_end(12)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        text_box.append(&text_label);

        // Wrap text_box in an ActionRow for consistent styling
        let content_row = adw::ActionRow::builder().visible(false).build();
        content_row.set_child(Some(&text_box));

        // ── Spinner + Generate button ─────────────────────────────────────────
        let generate_row = adw::ActionRow::builder()
            .title("Morning briefing")
            .subtitle("Checks today's readiness against your plan")
            .build();

        let spinner = gtk::Spinner::builder().visible(false).build();
        generate_row.add_prefix(&spinner);

        // Plain pill — the Today hero keeps the page's single primary action.
        let generate_btn = gtk::Button::builder()
            .label("Generate")
            .css_classes(["pill"])
            .valign(gtk::Align::Center)
            .tooltip_text("Ask the AI coach for today's briefing")
            .build();
        generate_row.add_suffix(&generate_btn);
        generate_row.set_activatable_widget(Some(&generate_btn));

        // Generate on top, then the decision, then the full briefing text.
        group.add(&generate_row);
        group.add(&action_row);
        group.add(&content_row);

        // Shared state: name of the AI-suggested alternative workout (Modify decision)
        let pending_alt_name: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

        // Wire up "Replace" (Modify) — swaps today's calendar entry for the
        // suggested alternative; the Today hero then offers Start on it.
        {
            let pool_u = pool.clone();
            let rt_u = rt_handle.clone();
            let rh = Rc::clone(&reload_holder);
            let alt_name = Rc::clone(&pending_alt_name);
            let today = today_str.clone();
            use_workout_btn.connect_clicked(move |btn| {
                let name = match alt_name.borrow().clone() {
                    Some(n) if !n.is_empty() => n,
                    _ => return,
                };
                let pool_u = pool_u.clone();
                let rh = Rc::clone(&rh);
                let today = today.clone();
                let btn = btn.clone();
                crate::ui::spawn_to_main(
                    &rt_u,
                    async move {
                        let workouts = match db::load_workouts(&pool_u).await {
                            Ok(w) => w,
                            Err(e) => {
                                tracing::error!("load_workouts for alt swap: {e}");
                                return None;
                            }
                        };
                        // Case-insensitive match — the AI may return a different casing
                        let Some(found) =
                            workouts.iter().find(|w| w.name.eq_ignore_ascii_case(&name))
                        else {
                            tracing::warn!("Alternative workout '{name}' not found in library");
                            return None;
                        };
                        let alt = found.clone();
                        // Swap the calendar entry so today reflects what was actually done
                        if let Ok(Some(entry)) = db::load_today_entry(&pool_u, &today).await {
                            if let Err(e) =
                                db::delete_today_calendar_entry(&pool_u, entry.workout.id, &today)
                                    .await
                            {
                                tracing::error!("delete_today_calendar_entry (swap): {e}");
                            }
                        }
                        if let Err(e) = db::schedule_workout(&pool_u, alt.id, &today).await {
                            tracing::error!("schedule_workout (alt swap): {e}");
                        }
                        Some(alt)
                    },
                    move |result| {
                        if result.is_some() {
                            btn.set_visible(false);
                            if let Some(reload) = rh.borrow().as_ref() {
                                reload();
                            }
                        }
                    },
                );
            });
        }

        // Wire up "Remove from calendar" (Rest)
        {
            let pool_r = pool.clone();
            let rt_r = rt_handle.clone();
            let rh = Rc::clone(&reload_holder);
            let today = today_str.clone();
            remove_btn.connect_clicked(move |btn| {
                let pool = pool_r.clone();
                let rt = rt_r.clone();
                let rh = Rc::clone(&rh);
                let today = today.clone();
                crate::ui::widgets::dialog::confirm_destructive(
                    btn,
                    "Remove Today's Workout?",
                    "This will remove today's scheduled workout so you can rest as recommended.",
                    "_Remove",
                    move || {
                        let pool = pool.clone();
                        let rh = Rc::clone(&rh);
                        let today = today.clone();
                        crate::ui::spawn_to_main(
                            &rt,
                            async move {
                                // Load today's workout id first, then remove it.
                                if let Ok(Some(entry)) = db::load_today_entry(&pool, &today).await {
                                    if let Err(e) = db::delete_today_calendar_entry(
                                        &pool,
                                        entry.workout.id,
                                        &today,
                                    )
                                    .await
                                    {
                                        tracing::error!("delete_today_calendar_entry failed: {e}");
                                    }
                                }
                            },
                            move |()| {
                                if let Some(reload) = rh.borrow().as_ref() {
                                    reload();
                                }
                            },
                        );
                    },
                );
            });
        }

        // Wire up Generate button
        {
            let pool_g = pool.clone();
            let rt_g = rt_handle.clone();
            let athlete_g = athlete.clone();
            let text_label_g = text_label.clone();
            let action_row_g = action_row.clone();
            let content_row_g = content_row.clone();
            let decision_badge_g = decision_badge.clone();
            let use_workout_btn_g = use_workout_btn.clone();
            let remove_btn_g = remove_btn.clone();
            let spinner_g = spinner.clone();
            let today_g = today_str.clone();
            let alt_name_g = Rc::clone(&pending_alt_name);

            generate_btn.connect_clicked(move |btn| {
                let api_key = match keystore::get_secret(keystore::KEY_ANTHROPIC) {
                    Ok(Some(k)) if !k.trim().is_empty() => k,
                    _ => {
                        tracing::warn!("Morning briefing: no AI provider key configured");
                        return;
                    }
                };

                // Get ICU credentials synchronously (fast local D-Bus calls, not network).
                let icu_api_key = keystore::get_secret(keystore::KEY_INTERVALS_API)
                    .unwrap_or(None)
                    .unwrap_or_default();

                btn.set_sensitive(false);
                spinner_g.set_visible(true);
                spinner_g.start();
                text_label_g.set_markup("Generating briefing…");
                text_label_g.set_visible(true);
                content_row_g.set_visible(true);

                // Channel carries (briefing_text, planned_name_for_align, has_planned).
                let (tx, rx) =
                    async_channel::bounded::<Result<(String, Option<String>, bool), String>>(1);

                let pool_t = pool_g.clone();
                let athlete_t = athlete_g.clone();
                let today_t = today_g.clone();

                // All network and DB work happens in the tokio runtime — the GTK main
                // thread is never blocked, so the UI stays responsive even with no internet.
                rt_g.spawn(async move {
                    let icu_athlete_id = db::get_setting(&pool_t, "intervals.athlete_id")
                        .await
                        .unwrap_or(None)
                        .unwrap_or_default();

                    // Sync intervals.icu — errors are non-fatal; we just skip stale data.
                    if !icu_api_key.trim().is_empty() && !icu_athlete_id.trim().is_empty() {
                        let today = chrono::Local::now().date_naive();
                        let thirty_ago = today - chrono::Duration::days(30);
                        let seven_ago = today - chrono::Duration::days(7);

                        if let Ok(acts) = crate::ai::intervals::fetch_activities(
                            &icu_athlete_id,
                            &icu_api_key,
                            thirty_ago,
                            today,
                        )
                        .await
                        {
                            for a in acts {
                                let _ = db::upsert_intervals_activity(
                                    &pool_t,
                                    &a.id,
                                    a.start_date_local,
                                    &a.name,
                                    a.icu_training_load,
                                    a.moving_time,
                                    a.average_watts,
                                    a.normalized_watts,
                                    a.average_hr,
                                    a.max_hr,
                                    &a.sport_type,
                                    a.start_datetime_local,
                                    a.distance_m,
                                    a.elevation_gain_m,
                                    a.average_cadence,
                                )
                                .await;
                            }
                            // A ride recorded in-app can arrive back here after a round
                            // trip through Garmin or Strava — link the two so it is shown
                            // and counted once.
                            if let Err(e) = crate::data::db::reconcile_icu_links(&pool_t).await {
                                tracing::error!("reconcile_icu_links: {e}");
                            }
                        }

                        if let Ok(wellness) = crate::ai::intervals::fetch_wellness(
                            &icu_athlete_id,
                            &icu_api_key,
                            seven_ago,
                            today,
                        )
                        .await
                        {
                            for w in wellness {
                                let entry = db::WellnessEntry {
                                    date: w.date,
                                    hrv: w.hrv,
                                    resting_hr: w.resting_hr,
                                    sleep_secs: w.sleep_secs,
                                    sleep_score: w.sleep_score,
                                    steps: w.steps,
                                    calories: w.calories,
                                };
                                let _ = db::upsert_wellness_entry(&pool_t, &entry).await;
                            }
                        }
                    }

                    let today_entry = db::load_today_entry(&pool_t, &today_t)
                        .await
                        .unwrap_or(None);
                    // Whether a workout is actually on today's calendar — the
                    // decision copy must not say "as planned" when nothing is.
                    let has_planned = today_entry.is_some();
                    let records = db::load_session_records(&pool_t).await.unwrap_or_default();
                    let intervals_pairs = db::load_intervals_tss_pairs(&pool_t)
                        .await
                        .unwrap_or_default();
                    let wellness_raw = db::load_wellness_recent(&pool_t, 1)
                        .await
                        .unwrap_or_default();
                    let workouts = db::load_workouts(&pool_t).await.unwrap_or_default();
                    let athlete_context = db::get_setting(&pool_t, "coaching.athlete_context")
                        .await
                        .unwrap_or(None)
                        .unwrap_or_default();

                    let ftp = athlete_t.ftp_watts;
                    let today_naive = chrono::Local::now().date_naive();
                    let (ctl, atl) = compute_ctl_atl(&records, &intervals_pairs, ftp, today_naive);
                    let tsb = ctl - atl;

                    let today_wellness = wellness_raw.first().map(|w| WellnessSnapshot {
                        date: w.date.format("%Y-%m-%d").to_string(),
                        hrv: w.hrv,
                        resting_hr: w.resting_hr,
                        sleep_hours: w.sleep_secs.map(|s| s as f32 / 3600.0),
                        sleep_score: w.sleep_score,
                        steps: w.steps,
                        calories: w.calories,
                    });

                    // If no workout is scheduled today, fall back to the cached AI coaching
                    // suggestion so the Morning Brief and Coaching tab stay in sync.
                    let planned_workout = if let Some(e) = today_entry {
                        Some(PlannedWorkout {
                            name: e.workout.name.clone(),
                            duration_mins: e.workout.duration_secs / 60,
                            tss: e.workout.tss,
                            category: e.workout.category.label().to_string(),
                        })
                    } else {
                        let cached_name = db::get_setting(&pool_t, "ai.suggestion_workout_name")
                            .await
                            .unwrap_or(None)
                            .unwrap_or_default();
                        if !cached_name.trim().is_empty() {
                            workouts
                                .iter()
                                .find(|w| w.name.eq_ignore_ascii_case(&cached_name))
                                .map(|w| PlannedWorkout {
                                    name: w.name.clone(),
                                    duration_mins: w.duration_secs / 60,
                                    tss: w.tss,
                                    category: w.category.label().to_string(),
                                })
                        } else {
                            None
                        }
                    };

                    let workout_options: Vec<WorkoutOption> = workouts
                        .iter()
                        .map(|w| WorkoutOption {
                            name: w.name.clone(),
                            duration_mins: w.duration_secs / 60,
                            tss: w.tss,
                            category: w.category.label().to_string(),
                        })
                        .collect();

                    let planned_name_for_align = planned_workout.as_ref().map(|p| p.name.clone());

                    let two_weeks_out = today_naive + chrono::Duration::days(14);
                    let start_str = today_naive.format("%Y-%m-%d").to_string();
                    let end_str = two_weeks_out.format("%Y-%m-%d").to_string();
                    let time_off_dates: Vec<String> =
                        db::load_time_off_between(&pool_t, &start_str, &end_str)
                            .await
                            .unwrap_or_default()
                            .into_iter()
                            .map(|e| e.date.format("%Y-%m-%d").to_string())
                            .collect();

                    let ctx = BriefingContext {
                        athlete: athlete_t,
                        ctl,
                        atl,
                        tsb,
                        today_wellness,
                        planned_workout,
                        workout_options,
                        athlete_context,
                        time_off_dates,
                    };
                    let prompt = build_briefing_prompt(&ctx);

                    let r = get_suggestion(&api_key, &prompt, 1200)
                        .await
                        .map(|text| (text, planned_name_for_align, has_planned))
                        .map_err(|e| e.to_string());
                    let _ = tx.send(r).await;
                });

                let text_l = text_label_g.clone();
                let action_r = action_row_g.clone();
                let content_r = content_row_g.clone();
                let badge = decision_badge_g.clone();
                let use_btn = use_workout_btn_g.clone();
                let rem_btn = remove_btn_g.clone();
                let spinner_c = spinner_g.clone();
                let btn_c = btn.clone();
                let pool_c = pool_g.clone();
                let rt_c = rt_g.clone();
                let today_c = today_g.clone();
                let alt_name_c = Rc::clone(&alt_name_g);
                glib::MainContext::default().spawn_local(async move {
                    if let Ok(result) = rx.recv().await {
                        match result {
                            Ok((text, planned_name_for_align, has_planned)) => {
                                let decision = parse_briefing_decision(&text);
                                let alt = parse_alternative_workout(&text);
                                *alt_name_c.borrow_mut() = alt.clone();
                                Self::apply_briefing(
                                    &text_l,
                                    &action_r,
                                    &content_r,
                                    &badge,
                                    &use_btn,
                                    &rem_btn,
                                    &text,
                                    &decision,
                                    alt.as_deref(),
                                    has_planned,
                                );
                                // Align the coaching suggestion with the briefing decision
                                let suggest_name = match &decision {
                                    BriefingDecision::Proceed => {
                                        planned_name_for_align.unwrap_or_default()
                                    }
                                    BriefingDecision::Modify => alt.unwrap_or_default(),
                                    BriefingDecision::Rest => String::new(),
                                };
                                // Cache the result and persist aligned suggestion
                                rt_c.spawn(async move {
                                    let _ =
                                        db::set_setting(&pool_c, "ai.morning_briefing_text", &text)
                                            .await;
                                    let _ = db::set_setting(
                                        &pool_c,
                                        "ai.morning_briefing_date",
                                        &today_c,
                                    )
                                    .await;
                                    if !suggest_name.is_empty() {
                                        let _ = db::set_setting(
                                            &pool_c,
                                            "ai.suggestion_workout_name",
                                            &suggest_name,
                                        )
                                        .await;
                                    }
                                });
                            }
                            Err(e) => {
                                tracing::error!("Morning briefing API error: {e}");
                            }
                        }
                    }
                    spinner_c.stop();
                    spinner_c.set_visible(false);
                    btn_c.set_sensitive(true);
                });
            });
        }

        // Load any cached briefing off the main thread (CLAUDE.md §2.3). On arrival,
        // either restore today's cached briefing or auto-generate a fresh one.
        {
            let pool_cache = pool.clone();
            let today_c = today_str.clone();
            let today_q = today_str.clone();
            let text_label_c = text_label.clone();
            let action_row_c = action_row.clone();
            let content_row_c = content_row.clone();
            let decision_badge_c = decision_badge.clone();
            let use_workout_btn_c = use_workout_btn.clone();
            let remove_btn_c = remove_btn.clone();
            let pending_alt_c = Rc::clone(&pending_alt_name);
            let generate_btn_c = generate_btn.clone();
            crate::ui::spawn_to_main(
                &rt_handle,
                async move {
                    let cached_text = db::get_setting(&pool_cache, "ai.morning_briefing_text")
                        .await
                        .unwrap_or(None)
                        .unwrap_or_default();
                    let cached_date = db::get_setting(&pool_cache, "ai.morning_briefing_date")
                        .await
                        .unwrap_or(None)
                        .unwrap_or_default();
                    let has_planned = db::load_today_entry(&pool_cache, &today_q)
                        .await
                        .unwrap_or(None)
                        .is_some();
                    (cached_text, cached_date, has_planned)
                },
                move |(cached_text, cached_date, has_planned)| {
                    if !cached_text.is_empty() && cached_date == today_c {
                        let decision = parse_briefing_decision(&cached_text);
                        let alt = parse_alternative_workout(&cached_text);
                        *pending_alt_c.borrow_mut() = alt.clone();
                        Self::apply_briefing(
                            &text_label_c,
                            &action_row_c,
                            &content_row_c,
                            &decision_badge_c,
                            &use_workout_btn_c,
                            &remove_btn_c,
                            &cached_text,
                            &decision,
                            alt.as_deref(),
                            has_planned,
                        );
                    } else {
                        // No briefing cached for today — auto-generate one.
                        generate_btn_c.emit_by_name::<()>("clicked", &[]);
                    }
                },
            );
        }

        group
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_briefing(
        text_label: &gtk::Label,
        action_row: &adw::ActionRow,
        content_row: &adw::ActionRow,
        decision_badge: &gtk::Label,
        use_workout_btn: &gtk::Button,
        remove_btn: &gtk::Button,
        text: &str,
        decision: &BriefingDecision,
        alt_workout: Option<&str>,
        has_planned: bool,
    ) {
        // Strip DECISION: and ALTERNATIVE_WORKOUT: lines from displayed text
        let cleaned: String = text
            .lines()
            .filter(|l| {
                let u = l.trim().to_uppercase();
                !u.starts_with("DECISION:") && !u.starts_with("ALTERNATIVE_WORKOUT:")
            })
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string();

        text_label.set_markup(&to_pango(&cleaned));
        text_label.set_visible(true);
        content_row.set_visible(true);
        action_row.set_visible(true);

        match decision {
            BriefingDecision::Proceed => {
                // "As planned" only makes sense when something is on the calendar.
                if has_planned {
                    action_row.set_title("Proceed as planned");
                    action_row.set_subtitle("Your readiness supports today's workout.");
                } else {
                    action_row.set_title("Ready to train");
                    action_row.set_subtitle("Your readiness supports training today.");
                }
                decision_badge.set_label("Proceed");
                decision_badge.set_css_classes(&["pill", "caption", "success"]);
                use_workout_btn.set_visible(false);
                remove_btn.set_visible(false);
            }
            BriefingDecision::Modify => {
                let label = if let Some(name) = alt_workout {
                    format!("Modify → {name}")
                } else {
                    "Modify".to_string()
                };
                action_row.set_title(&label);
                action_row.set_subtitle("Your readiness suggests an easier alternative today.");
                decision_badge.set_label("Modify");
                decision_badge.set_css_classes(&["pill", "caption", "warning"]);
                use_workout_btn.set_visible(alt_workout.is_some());
                remove_btn.set_visible(false);
            }
            BriefingDecision::Rest => {
                action_row.set_title("Rest today");
                action_row.set_subtitle("Recovery takes priority over training today.");
                decision_badge.set_label("Rest");
                decision_badge.set_css_classes(&["pill", "caption", "error"]);
                use_workout_btn.set_visible(false);
                // Nothing to remove when nothing is scheduled.
                remove_btn.set_visible(has_planned);
            }
        }
    }

    /// The Today hero — exactly one of three states:
    /// a scheduled workout (Start), the AI suggestion standing in for an
    /// empty day (Schedule / Start), or a rest day linking to the Calendar.
    #[allow(clippy::too_many_arguments)]
    fn build_workout_card(
        entry: Option<db::TodayEntry>,
        suggested: Option<Workout>,
        ai_detail: &str,
        on_start: Rc<dyn Fn(Workout)>,
        athlete: &AthleteProfile,
        on_open_calendar: Rc<dyn Fn()>,
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        reload_holder: ReloadHolder,
        today_str: String,
    ) -> gtk::Box {
        let outer = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .build();

        outer.append(
            &gtk::Label::builder()
                .label("Today")
                .halign(gtk::Align::Start)
                .css_classes(["heading"])
                .build(),
        );

        match (entry, suggested) {
            // ── Scheduled workout ────────────────────────────────────────────
            (Some(entry), _) => {
                let subtitle = if entry.workout.description.trim().is_empty() {
                    Self::workout_meta(&entry.workout)
                } else {
                    format!(
                        "{} — {}",
                        Self::workout_meta(&entry.workout),
                        entry.workout.description.trim()
                    )
                };
                let (card, action_area) = Self::hero_card(&entry.workout, athlete, &subtitle);

                if entry.completed {
                    let done = gtk::Box::builder()
                        .orientation(gtk::Orientation::Horizontal)
                        .spacing(6)
                        .valign(gtk::Align::Center)
                        .build();
                    done.append(
                        &gtk::Image::builder()
                            .icon_name("object-select-symbolic")
                            .css_classes(["success"])
                            .build(),
                    );
                    done.append(
                        &gtk::Label::builder()
                            .label("Completed")
                            .css_classes(["success", "caption-heading"])
                            .build(),
                    );
                    action_area.append(&done);
                } else {
                    let start_btn = gtk::Button::builder()
                        .label("Start")
                        .css_classes(["suggested-action", "pill"])
                        .valign(gtk::Align::Center)
                        .tooltip_text("Start today's scheduled workout")
                        .build();
                    let w = entry.workout.clone();
                    start_btn.connect_clicked(move |_| on_start(w.clone()));
                    action_area.append(&start_btn);
                }

                outer.append(&card);
            }
            // ── Nothing scheduled — the AI suggestion becomes the hero ───────
            (None, Some(workout)) => {
                let subtitle = if ai_detail.is_empty() {
                    Self::workout_meta(&workout)
                } else {
                    format!("{} — {}", Self::workout_meta(&workout), ai_detail)
                };
                let (card, action_area) = Self::hero_card(&workout, athlete, &subtitle);

                // Schedule: put the suggestion on today's calendar, then reload
                // so this card becomes the scheduled hero.
                let schedule_btn = gtk::Button::builder()
                    .label("Schedule")
                    .css_classes(["pill"])
                    .valign(gtk::Align::Center)
                    .tooltip_text("Add this workout to today's calendar")
                    .build();
                {
                    let pool = pool.clone();
                    let workout_id = workout.id;
                    schedule_btn.connect_clicked(move |_| {
                        let pool = pool.clone();
                        let today = today_str.clone();
                        let rh = Rc::clone(&reload_holder);
                        crate::ui::spawn_to_main(
                            &rt_handle,
                            async move {
                                if let Err(e) =
                                    db::schedule_workout(&pool, workout_id, &today).await
                                {
                                    tracing::error!("schedule suggested workout: {e}");
                                }
                            },
                            move |()| {
                                if let Some(reload) = rh.borrow().as_ref() {
                                    reload();
                                }
                            },
                        );
                    });
                }
                action_area.append(&schedule_btn);

                // Start: the day's primary action when nothing else is planned.
                let start_btn = gtk::Button::builder()
                    .label("Start")
                    .css_classes(["suggested-action", "pill"])
                    .valign(gtk::Align::Center)
                    .tooltip_text("Start the suggested workout now")
                    .build();
                start_btn.connect_clicked(move |_| on_start(workout.clone()));
                action_area.append(&start_btn);

                outer.append(&card);
            }
            // ── Rest day ─────────────────────────────────────────────────────
            (None, None) => {
                let list = gtk::ListBox::builder()
                    .css_classes(["boxed-list"])
                    .selection_mode(gtk::SelectionMode::None)
                    .build();
                // The row performs the action its copy names: click → Calendar.
                let row = adw::ActionRow::builder()
                    .title("Rest Day")
                    .subtitle("No workout scheduled — plan one in Calendar")
                    .activatable(true)
                    .tooltip_text("Open Calendar to plan a workout")
                    .build();
                row.add_prefix(
                    &gtk::Image::builder()
                        .icon_name("weather-clear-night-symbolic")
                        .css_classes(["dim-label"])
                        .build(),
                );
                row.add_suffix(
                    &gtk::Image::builder()
                        .icon_name("go-next-symbolic")
                        .css_classes(["dim-label"])
                        .build(),
                );
                row.connect_activated(move |_| on_open_calendar());
                list.append(&row);
                outer.append(&list);
            }
        }

        outer
    }

    /// Meta line shared by hero cards: duration · TSS · category.
    fn workout_meta(w: &Workout) -> String {
        format!(
            "{} min · TSS {} · {}",
            w.duration_secs / 60,
            w.tss as u32,
            w.category.label()
        )
    }

    /// Card with the workout's zone-coloured profile, name, and subtitle.
    /// Returns `(card, action_area)` — the caller appends its own buttons.
    fn hero_card(
        workout: &Workout,
        athlete: &AthleteProfile,
        subtitle: &str,
    ) -> (gtk::Box, gtk::Box) {
        let card = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["card"])
            .build();

        let graph = WorkoutGraph::new(workout, athlete);
        graph.widget().set_content_height(72);
        graph.widget().set_margin_top(12);
        graph.widget().set_margin_start(12);
        graph.widget().set_margin_end(12);
        card.append(graph.widget());

        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        let text_col = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .hexpand(true)
            .build();
        text_col.append(
            &gtk::Label::builder()
                .label(&workout.name)
                .halign(gtk::Align::Start)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .css_classes(["title-2"])
                .build(),
        );
        text_col.append(
            &gtk::Label::builder()
                .label(subtitle)
                .halign(gtk::Align::Start)
                .wrap(true)
                .xalign(0.0)
                .css_classes(["caption", "dim-label"])
                .build(),
        );
        row.append(&text_col);
        card.append(&row);

        (card, row)
    }

    /// One quiet list holding form (readiness + CTL/ATL/TSB) and the most
    /// recent activity — replaces the old banner + two-card row.
    #[allow(clippy::too_many_arguments)]
    fn build_status_list(
        ctl: f64,
        atl: f64,
        tsb: f64,
        ctl_7d: f64,
        atl_7d: f64,
        insight: &str,
        record: Option<&db::SessionRecord>,
        ftp: u32,
        on_view_fitness: Rc<dyn Fn()>,
        on_open_calendar: Rc<dyn Fn()>,
    ) -> gtk::ListBox {
        let list = gtk::ListBox::builder()
            .css_classes(["boxed-list"])
            .selection_mode(gtk::SelectionMode::None)
            .build();

        // ── Form row: readiness in plain words, numbers as quiet pills ───────
        let message = if tsb > 5.0 {
            "Fresh — ready to train hard"
        } else if tsb > -10.0 {
            "Normal training fatigue — moderate effort is fine"
        } else if tsb > -20.0 {
            "Elevated fatigue — consider an easier session"
        } else {
            "Very fatigued — rest is the priority"
        };

        let form_row = adw::ActionRow::builder()
            .title(message)
            .activatable(true)
            .tooltip_text("Open Fitness for the full picture")
            // AI text may contain markup-significant characters (& etc.)
            .use_markup(false)
            .build();
        let preview = Self::insight_preview(insight);
        if !preview.is_empty() {
            form_row.set_subtitle(&preview);
        }

        let trend_arrow = |current: f64, prev: f64| -> &'static str {
            if current > prev + 1.0 {
                "↑"
            } else if current < prev - 1.0 {
                "↓"
            } else {
                "→"
            }
        };

        // CTL and ATL only — the readiness message above already expresses
        // their balance (TSB), so a third number would say nothing new.
        for (label, value, trend, tip) in [
            (
                "CTL",
                format!("{:.0}", ctl),
                trend_arrow(ctl, ctl_7d),
                "Chronic Training Load — 42-day fitness average. Higher means more aerobic base built up.",
            ),
            (
                "ATL",
                format!("{:.0}", atl),
                trend_arrow(atl, atl_7d),
                "Acute Training Load — 7-day fatigue average. Spikes after hard weeks.",
            ),
        ] {
            let pair = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(6)
                .tooltip_text(tip)
                .valign(gtk::Align::Center)
                .build();
            pair.append(
                &gtk::Label::builder()
                    .label(label)
                    .css_classes(["caption", "dim-label"])
                    .build(),
            );
            pair.append(
                &gtk::Label::builder()
                    .label(format!("{value}{trend}"))
                    .css_classes(["caption-heading", "numeric"])
                    .build(),
            );
            form_row.add_suffix(&pair);
        }

        form_row.add_suffix(
            &gtk::Image::builder()
                .icon_name("go-next-symbolic")
                .css_classes(["dim-label"])
                .build(),
        );
        form_row.connect_activated(move |_| on_view_fitness());
        list.append(&form_row);

        // ── Last activity row ────────────────────────────────────────────────
        let last_row = match record {
            None => adw::ActionRow::builder()
                .title("No sessions recorded yet")
                .subtitle("Complete a workout to see it here")
                .build(),
            // Workout names are user/AI-supplied — markup disabled below.
            Some(r) => {
                let local_dt = r.session.started_at.with_timezone(&Local);
                let title = r.workout_name.as_deref().unwrap_or("Free Ride");
                let mins = r.session.duration_secs() as u32 / 60;
                let power_str = match r.session.normalised_power() {
                    Some(np) => format!("{} W NP", np as u32),
                    None => match r.session.average_power() {
                        Some(avg) => format!("{} W avg", avg as u32),
                        None => String::new(),
                    },
                };
                let tss_str = r
                    .session
                    .tss(ftp)
                    .map(|t| format!(" · TSS {:.0}", t))
                    .unwrap_or_default();
                let detail = if power_str.is_empty() {
                    format!("{} min{}", mins, tss_str)
                } else {
                    format!("{} min · {}{}", mins, power_str, tss_str)
                };
                adw::ActionRow::builder()
                    .title(title)
                    .use_markup(false)
                    .subtitle(format!("{} — {}", local_dt.format("%-d %b"), detail))
                    .build()
            }
        };
        last_row.add_prefix(
            &gtk::Image::builder()
                .icon_name("document-open-recent-symbolic")
                .css_classes(["dim-label"])
                .build(),
        );
        // Past sessions live in the Calendar — the row takes you to them.
        if record.is_some() {
            last_row.set_activatable(true);
            last_row.set_tooltip_text(Some("Open Calendar to review past sessions"));
            last_row.add_suffix(
                &gtk::Image::builder()
                    .icon_name("go-next-symbolic")
                    .css_classes(["dim-label"])
                    .build(),
            );
            last_row.connect_activated(move |_| on_open_calendar());
        }
        list.append(&last_row);

        list
    }

    /// First sentence of the AI fitness insight, markdown stripped, ≤120 chars.
    fn insight_preview(insight: &str) -> String {
        if insight.is_empty() {
            return String::new();
        }
        let plain = strip_markdown(insight);
        let first_sentence = plain
            .split(['.', '\n'])
            .find(|s| !s.trim().is_empty())
            .unwrap_or("")
            .trim()
            .to_string();
        if first_sentence.is_empty() {
            String::new()
        } else if first_sentence.chars().count() > 120 {
            let cut = first_sentence
                .char_indices()
                .nth(120)
                .map(|(i, _)| i)
                .unwrap_or(first_sentence.len());
            format!("{}…", &first_sentence[..cut])
        } else {
            format!("{}.", first_sentence)
        }
    }
}

fn compute_ctl_atl(
    records: &[db::SessionRecord],
    intervals_pairs: &[(NaiveDate, f32)],
    ftp: u32,
    today: NaiveDate,
) -> (f64, f64) {
    let mut daily_tss: HashMap<NaiveDate, f32> = HashMap::new();
    for r in records {
        if r.counted_via_intervals() {
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
