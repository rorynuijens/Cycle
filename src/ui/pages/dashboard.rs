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

        let greeting_label = gtk::Label::builder()
            .label("")
            .halign(gtk::Align::Start)
            .css_classes(["title-1"])
            .build();

        let subtitle_label = gtk::Label::builder()
            .label("")
            .halign(gtk::Align::Start)
            .css_classes(["dim-label"])
            .build();

        inner.append(&greeting_label);
        inner.append(&subtitle_label);

        // ── Morning Briefing card (static — not rebuilt on each reload) ──────
        let reload_holder: ReloadHolder = Rc::new(RefCell::new(None));
        let briefing_card = Self::build_briefing_card(
            pool.clone(),
            rt_handle.clone(),
            athlete.clone(),
            Rc::clone(&reload_holder),
            Rc::clone(&on_start),
        );
        inner.append(&briefing_card);

        // Dynamic area rebuilt on each reload
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
            let dynamic = dynamic.clone();
            let pool = pool.clone();
            let rt_handle = rt_handle.clone();
            let ftp = athlete.ftp_watts;
            let athlete_name = athlete.name.clone();
            let on_start = Rc::clone(&on_start);
            let on_view_fitness = Rc::clone(&on_view_fitness);

            Rc::new(move || {
                let now = Local::now();

                let salutation = match now.hour() {
                    5..=11 => "Good morning",
                    12..=17 => "Good afternoon",
                    _ => "Good evening",
                };
                greeting_label.set_label(&format!("{}, {}!", salutation, athlete_name));

                let today_naive = now.date_naive();
                let weekday = today_naive.format("%A, %-d %B %Y").to_string();
                subtitle_label.set_label(&weekday);

                let today_str = today_naive.format("%Y-%m-%d").to_string();

                // Load all data needed for the summary cards
                let today_entry = rt_handle
                    .block_on(db::load_today_entry(&pool, &today_str))
                    .unwrap_or(None);

                let records = rt_handle
                    .block_on(db::load_session_records(&pool))
                    .unwrap_or_default();

                let ai_workout_name = rt_handle
                    .block_on(db::get_setting(&pool, "ai.suggestion_workout_name"))
                    .unwrap_or(None)
                    .unwrap_or_default();
                let ai_workout_detail = rt_handle
                    .block_on(db::get_setting(&pool, "ai.suggestion_workout_detail"))
                    .unwrap_or(None)
                    .unwrap_or_default();

                let ai_fitness_insight = rt_handle
                    .block_on(db::get_setting(&pool, "ai.fitness_insight"))
                    .unwrap_or(None)
                    .unwrap_or_default();

                let intervals_pairs = rt_handle
                    .block_on(db::load_intervals_tss_pairs(&pool))
                    .unwrap_or_default();

                // Compute TSB for readiness + 7-day trend
                let (ctl, atl) = compute_ctl_atl(&records, &intervals_pairs, ftp, today_naive);
                let tsb = ctl - atl;
                let week_ago = today_naive - Duration::days(7);
                let (ctl_7d, atl_7d) = compute_ctl_atl(&records, &intervals_pairs, ftp, week_ago);
                let tsb_7d = ctl_7d - atl_7d;

                while let Some(child) = dynamic.first_child() {
                    dynamic.remove(&child);
                }

                // ── First-run onboarding ─────────────────────────────────────
                let first_use_done = rt_handle
                    .block_on(db::get_setting(&pool, "first_use_complete"))
                    .unwrap_or(None)
                    .map(|v| v == "1")
                    .unwrap_or(false);

                if !first_use_done && records.is_empty() && today_entry.is_none() && ctl == 0.0 {
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

                    let pool_ob = pool.clone();
                    let rt_ob = rt_handle.clone();
                    let dynamic_ob = dynamic.clone();
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
                                // dashboard reloads to the normal view on next visit.
                                while let Some(child) = dynamic_w.first_child() {
                                    dynamic_w.remove(&child);
                                }
                            }),
                        );
                    });

                    dynamic.append(&status);
                    return;
                }

                // ── TSB status banner ─────────────────────────────────────────
                let banner = Self::build_tsb_banner(tsb);
                let vf = Rc::clone(&on_view_fitness);
                banner.connect_button_clicked(move |_| vf());
                dynamic.append(&banner);

                // ── Today's workout + AI suggestion (consolidated) ────────────
                dynamic.append(&Self::build_workout_card(
                    today_entry,
                    &ai_workout_name,
                    &ai_workout_detail,
                    Rc::clone(&on_start),
                ));

                // ── Two-column row: Last Activity | Fitness ───────────────────
                let last_session = records.first();
                let cards = vec![
                    Self::build_last_activity_card(last_session, ftp),
                    Self::build_fitness_card(
                        ctl,
                        atl,
                        tsb,
                        ctl_7d,
                        atl_7d,
                        tsb_7d,
                        &ai_fitness_insight,
                    ),
                ];
                let flow = gtk::FlowBox::builder()
                    .column_spacing(12)
                    .row_spacing(12)
                    .max_children_per_line(2)
                    .min_children_per_line(1)
                    .selection_mode(gtk::SelectionMode::None)
                    .homogeneous(true)
                    .build();
                for card in &cards {
                    flow.append(card);
                }
                for i in 0..cards.len() as i32 {
                    if let Some(child) = flow.child_at_index(i) {
                        child.set_hexpand(true);
                    }
                }
                dynamic.append(&flow);
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
        on_start: Rc<dyn Fn(Workout)>,
    ) -> adw::PreferencesGroup {
        let today_str = Local::now().date_naive().format("%Y-%m-%d").to_string();

        let group = adw::PreferencesGroup::builder()
            .title("Morning Briefing")
            .description(
                "Reviews your planned workout against today's readiness (HRV, sleep, TSB). \
                 May suggest a different workout than the Coaching tab when today's fatigue \
                 or wellness warrants it.",
            )
            .build();

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

        // Modify: "Use this workout" button (hidden unless decision is Modify)
        let use_workout_btn = gtk::Button::builder()
            .label("Use this workout")
            .css_classes(["pill", "suggested-action"])
            .valign(gtk::Align::Center)
            .visible(false)
            .tooltip_text("Replace today's scheduled workout with the AI suggestion")
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

        group.add(&action_row);

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
        group.add(&content_row);

        // ── Spinner + Generate button ─────────────────────────────────────────
        let generate_row = adw::ActionRow::builder()
            .title("AI Coach Briefing")
            .subtitle("Reviews readiness and today's planned workout, may suggest alternatives")
            .build();

        let spinner = gtk::Spinner::builder().visible(false).build();
        generate_row.add_prefix(&spinner);

        let generate_btn = gtk::Button::builder()
            .label("Generate")
            .css_classes(["pill", "suggested-action"])
            .valign(gtk::Align::Center)
            .tooltip_text("Ask the AI coach for today's briefing")
            .build();
        generate_row.add_suffix(&generate_btn);
        generate_row.set_activatable_widget(Some(&generate_btn));
        group.add(&generate_row);

        // Shared state: name of the AI-suggested alternative workout (Modify decision)
        let pending_alt_name: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

        // Restore cached briefing if it's from today
        let cached_text = rt_handle
            .block_on(db::get_setting(&pool, "ai.morning_briefing_text"))
            .unwrap_or(None)
            .unwrap_or_default();
        let cached_date = rt_handle
            .block_on(db::get_setting(&pool, "ai.morning_briefing_date"))
            .unwrap_or(None)
            .unwrap_or_default();

        if !cached_text.is_empty() && cached_date == today_str {
            let decision = parse_briefing_decision(&cached_text);
            let alt = parse_alternative_workout(&cached_text);
            *pending_alt_name.borrow_mut() = alt.clone();
            Self::apply_briefing(
                &text_label,
                &action_row,
                &content_row,
                &decision_badge,
                &use_workout_btn,
                &remove_btn,
                &cached_text,
                &decision,
                alt.as_deref(),
            );
        }

        // Wire up "Use this workout" (Modify) — swaps today's calendar entry and loads it
        {
            let pool_u = pool.clone();
            let rt_u = rt_handle.clone();
            let rh = Rc::clone(&reload_holder);
            let alt_name = Rc::clone(&pending_alt_name);
            let today = today_str.clone();
            let on_start_u = Rc::clone(&on_start);
            use_workout_btn.connect_clicked(move |btn| {
                let name = match alt_name.borrow().clone() {
                    Some(n) if !n.is_empty() => n,
                    _ => return,
                };
                let pool_u = pool_u.clone();
                let rh = Rc::clone(&rh);
                let on_start_u = Rc::clone(&on_start_u);
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
                        if let Some(alt) = result {
                            btn.set_visible(false);
                            on_start_u(alt);
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

                // Channel carries (briefing_text, planned_name_for_align).
                let (tx, rx) =
                    async_channel::bounded::<Result<(String, Option<String>), String>>(1);

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
                        .map(|text| (text, planned_name_for_align))
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
                            Ok((text, planned_name_for_align)) => {
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

        // Auto-generate on first open of the day if there's no cached briefing yet
        if cached_text.is_empty() || cached_date != today_str {
            let btn = generate_btn.clone();
            glib::idle_add_local_once(move || {
                btn.emit_by_name::<()>("clicked", &[]);
            });
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
                action_row.set_title("Recommendation: Proceed");
                action_row.set_subtitle(
                    "Today's readiness supports the planned workout — go ahead as scheduled.",
                );
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
                action_row.set_subtitle(
                    "Today's fatigue or wellness data suggests a lower-intensity alternative \
                     than the Coaching tab's general recommendation.",
                );
                decision_badge.set_label("Modify");
                decision_badge.set_css_classes(&["pill", "caption", "warning"]);
                use_workout_btn.set_visible(alt_workout.is_some());
                remove_btn.set_visible(false);
            }
            BriefingDecision::Rest => {
                action_row.set_title("Recommendation: Rest");
                action_row.set_subtitle(
                    "Today's fatigue or wellness indicators suggest rest takes priority over \
                     any training, including the Coaching tab's suggestion.",
                );
                decision_badge.set_label("Rest");
                decision_badge.set_css_classes(&["pill", "caption", "error"]);
                use_workout_btn.set_visible(false);
                remove_btn.set_visible(true);
            }
        }
    }

    fn build_tsb_banner(tsb: f64) -> adw::Banner {
        let (message, css) = if tsb > 5.0 {
            (
                format!("Good form today — ready to train hard (TSB {:+.0})", tsb),
                "success",
            )
        } else if tsb > -10.0 {
            (
                format!(
                    "Normal training fatigue — moderate effort recommended (TSB {:+.0})",
                    tsb
                ),
                "",
            )
        } else if tsb > -20.0 {
            (
                format!(
                    "Elevated fatigue — consider an easier session today (TSB {:+.0})",
                    tsb
                ),
                "warning",
            )
        } else {
            (
                format!(
                    "Very fatigued — rest is the priority today (TSB {:+.0})",
                    tsb
                ),
                "error",
            )
        };

        let banner = adw::Banner::builder()
            .title(&message)
            .button_label("View Fitness")
            .revealed(true)
            .build();
        if !css.is_empty() {
            banner.add_css_class(css);
        }
        banner
    }

    fn build_workout_card(
        entry: Option<db::TodayEntry>,
        ai_name: &str,
        ai_detail: &str,
        on_start: Rc<dyn Fn(Workout)>,
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

        let list = gtk::ListBox::builder()
            .css_classes(["boxed-list"])
            .selection_mode(gtk::SelectionMode::None)
            .build();

        match entry {
            None => {
                let row = adw::ActionRow::builder()
                    .title("Rest Day")
                    .subtitle("No workout scheduled — open Calendar to plan ahead")
                    .build();
                row.add_suffix(
                    &gtk::Image::builder()
                        .icon_name("weather-clear-night-symbolic")
                        .build(),
                );
                list.append(&row);
            }
            Some(entry) => {
                let subtitle = if entry.workout.description.trim().is_empty() {
                    format!(
                        "{} min · TSS {} · {}",
                        entry.workout.duration_secs / 60,
                        entry.workout.tss as u32,
                        entry.workout.category.label()
                    )
                } else {
                    format!(
                        "{} min · TSS {} · {} — {}",
                        entry.workout.duration_secs / 60,
                        entry.workout.tss as u32,
                        entry.workout.category.label(),
                        entry.workout.description.trim()
                    )
                };
                let row = adw::ActionRow::builder()
                    .title(&entry.workout.name)
                    .subtitle(&subtitle)
                    .build();

                if entry.completed {
                    row.add_suffix(
                        &gtk::Image::builder()
                            .icon_name("object-select-symbolic")
                            .css_classes(["success"])
                            .build(),
                    );
                } else {
                    let start_btn = gtk::Button::builder()
                        .label("Start")
                        .css_classes(["suggested-action", "pill"])
                        .valign(gtk::Align::Center)
                        .tooltip_text("Start today's scheduled workout")
                        .build();
                    let w = entry.workout.clone();
                    start_btn.connect_clicked(move |_| on_start(w.clone()));
                    row.set_activatable_widget(Some(&start_btn));
                    row.add_suffix(&start_btn);
                }
                list.append(&row);
            }
        }

        // AI suggestion as a secondary row — visually separated with a badge
        if !ai_name.is_empty() {
            let ai_row = adw::ActionRow::builder()
                .title(ai_name)
                .subtitle(if ai_detail.is_empty() {
                    "AI suggested workout — open Coaching for details"
                } else {
                    ai_detail
                })
                .build();
            // Icon distinguishes AI row from scheduled workout row
            ai_row.add_prefix(
                &gtk::Image::builder()
                    .icon_name("chat-message-new-symbolic")
                    .css_classes(["accent"])
                    .build(),
            );
            // "Suggested" badge makes the source explicit at a glance
            let badge = gtk::Label::builder()
                .label("Suggested")
                .css_classes(["pill", "caption", "accent"])
                .valign(gtk::Align::Center)
                .build();
            ai_row.add_suffix(&badge);
            list.append(&ai_row);
        }

        outer.append(&list);
        outer
    }

    fn build_last_activity_card(record: Option<&db::SessionRecord>, ftp: u32) -> gtk::Box {
        match record {
            None => Self::summary_card(
                "Last Activity",
                "No sessions recorded yet",
                "Complete a workout to see it here",
                "document-open-recent-symbolic",
            ),
            Some(r) => {
                let local_dt = r.session.started_at.with_timezone(&Local);
                let title = r.workout_name.as_deref().unwrap_or("Free Ride");
                let dur = r.session.duration_secs() as u32;
                let mins = dur / 60;
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
                let subtitle = format!("{} — {}", local_dt.format("%-d %b"), detail);
                Self::summary_card(
                    "Last Activity",
                    title,
                    &subtitle,
                    "document-open-recent-symbolic",
                )
            }
        }
    }

    fn build_fitness_card(
        ctl: f64,
        atl: f64,
        tsb: f64,
        ctl_7d: f64,
        atl_7d: f64,
        tsb_7d: f64,
        insight: &str,
    ) -> gtk::Box {
        let frame = gtk::Box::builder()
            .css_classes(["card"])
            .hexpand(true)
            .build();

        let vbox = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        vbox.append(
            &gtk::Label::builder()
                .label("Fitness")
                .halign(gtk::Align::Start)
                .css_classes(["caption", "dim-label"])
                .build(),
        );

        let trend_arrow = |current: f64, prev: f64| -> &'static str {
            if current > prev + 1.0 {
                "↑"
            } else if current < prev - 1.0 {
                "↓"
            } else {
                "→"
            }
        };

        // CTL / ATL / TSB pill row
        let metrics_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .build();

        for (label, value, trend, class, tip) in [
            (
                "CTL",
                format!("{:.0}", ctl),
                trend_arrow(ctl, ctl_7d),
                "",
                "Chronic Training Load — 42-day fitness average. Higher means more aerobic base built up.",
            ),
            (
                "ATL",
                format!("{:.0}", atl),
                trend_arrow(atl, atl_7d),
                "",
                "Acute Training Load — 7-day fatigue average. Spikes after hard weeks.",
            ),
            (
                "TSB",
                format!("{:+.0}", tsb),
                trend_arrow(tsb, tsb_7d),
                if tsb > 5.0 {
                    "success"
                } else if tsb < -10.0 {
                    "warning"
                } else {
                    ""
                },
                "Training Stress Balance (CTL − ATL). Positive = fresh and ready; negative = accumulated fatigue.",
            ),
        ] {
            let pair = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(4)
                .tooltip_text(tip)
                .build();
            pair.append(
                &gtk::Label::builder()
                    .label(label)
                    .css_classes(["caption", "dim-label"])
                    .build(),
            );
            let val_label = gtk::Label::builder()
                .label(format!("{value}{trend}"))
                .css_classes(["caption-heading", "numeric"])
                .build();
            if !class.is_empty() {
                val_label.add_css_class(class);
            }
            pair.append(&val_label);
            metrics_box.append(&pair);
        }
        vbox.append(&metrics_box);

        // First sentence of AI insight (if any) — strip markdown for plain preview
        if !insight.is_empty() {
            let plain = strip_markdown(insight);
            let first_sentence = plain
                .split(['.', '\n'])
                .find(|s| !s.trim().is_empty())
                .unwrap_or("")
                .trim()
                .to_string();
            let preview = if first_sentence.chars().count() > 120 {
                let cut = first_sentence
                    .char_indices()
                    .nth(120)
                    .map(|(i, _)| i)
                    .unwrap_or(first_sentence.len());
                format!("{}…", &first_sentence[..cut])
            } else if first_sentence.is_empty() {
                String::new()
            } else {
                format!("{}.", first_sentence)
            };
            if !preview.is_empty() {
                vbox.append(
                    &gtk::Label::builder()
                        .label(&preview)
                        .css_classes(["caption", "dim-label"])
                        .halign(gtk::Align::Start)
                        .wrap(true)
                        .xalign(0.0)
                        .build(),
                );
            }
        }

        frame.append(&vbox);
        frame
    }

    /// Generic summary card with icon, title, value, and subtitle.
    fn summary_card(section: &str, title: &str, subtitle: &str, icon: &str) -> gtk::Box {
        let frame = gtk::Box::builder()
            .css_classes(["card"])
            .hexpand(true)
            .build();

        let vbox = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(14)
            .margin_end(14)
            .build();

        vbox.append(
            &gtk::Label::builder()
                .label(section)
                .halign(gtk::Align::Start)
                .css_classes(["caption", "dim-label"])
                .build(),
        );

        let title_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .build();

        if !icon.is_empty() {
            let icon_widget = gtk::Image::builder()
                .icon_name(icon)
                .icon_size(gtk::IconSize::Normal)
                .build();
            title_row.append(&icon_widget);
        }

        title_row.append(
            &gtk::Label::builder()
                .label(title)
                .halign(gtk::Align::Start)
                .hexpand(true)
                .css_classes(["heading"])
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build(),
        );
        vbox.append(&title_row);

        if !subtitle.is_empty() {
            vbox.append(
                &gtk::Label::builder()
                    .label(subtitle)
                    .halign(gtk::Align::Start)
                    .css_classes(["caption", "dim-label"])
                    .wrap(true)
                    .xalign(0.0)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .build(),
            );
        }

        frame.append(&vbox);
        frame
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
